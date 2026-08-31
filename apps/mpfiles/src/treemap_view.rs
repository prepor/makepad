//! The treemap: a spatial map of where a folder's bytes actually are.
//!
//! Every rectangle's area is its bytes, all the way down to the individual
//! file: a 4 GB video is visibly four thousand times the block a 1 MB photo
//! gets, and the folder it sits in is the frame drawn around it. There is no
//! depth limit — the map stops where a rectangle stops being visible, which on
//! a big window is at the file and on a small one is a few folders up.
//!
//! Three things make it readable rather than a field of colour. Each tile is a
//! shaded cushion (Van Wijk), so ten thousand rectangles read as ten thousand
//! things. Hue says what kind of thing it is, and a folder borrows the hue of
//! its own heaviest content, so a folder full of video reads as video without
//! being opened. And nesting is drawn as a frame that narrows with depth,
//! which is what keeps the borders from eating the bytes they surround.
//!
//! The scan never runs on the UI thread and never makes anyone wait for all of
//! it: a worker streams the tree back as it walks (see [`crate::treemap`]), the
//! map is drawable after the first `read_dir`, and it sharpens as the walk goes
//! deeper. The layout itself is pure arithmetic and runs inline, throttled
//! while a scan is still feeding it.

use makepad_widgets::makepad_platform::thread::SignalToUI;
use makepad_widgets::*;

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    model::FileKind,
    theme::Palette,
    treemap::{self, Cell, MapStyle, Node, Rect as MapRect, ScanStep},
};

/// The strip along the top carrying the zoom breadcrumb and the scan state.
const CRUMB_H: f64 = 21.0;
/// The strip along the bottom carrying the persistent selection readout.
const FOOT_H: f64 = 21.0;
/// A rectangle needs this much room before its name is worth drawing, and
/// this much again before its size goes on a second line.
const LABEL_MIN: DVec2 = DVec2 { x: 50.0, y: 19.0 };
const LABEL_TWO_LINE_H: f64 = 32.0;
/// A ceiling on labels per frame. Past a few hundred names nobody is reading
/// them and every one costs a text layout.
const LABEL_BUDGET: usize = 700;
/// The map is only re-laid-out this often while a scan is still feeding it —
/// the tree changes hundreds of times a second and the picture does not need
/// to.
const RELAYOUT_EVERY: Duration = Duration::from_millis(110);
/// How often the worker wakes the UI. The steps themselves queue freely; this
/// only bounds the signals.
const SIGNAL_EVERY: Duration = Duration::from_millis(45);
/// The kind tag [`treemap::layout`] gives the "N smaller items" rectangle.
const KIND_BUNDLE: u8 = u8::MAX;
/// The palette class everything unrecognised falls into.
const OTHER_CLASS: usize = 6;
/// The tile size at which the cushion is at full strength. A cushion lives in
/// the tile's own 0..1 space, so left alone a huge rectangle gets a huge soft
/// gradient that reads as a spotlight rather than as a surface. The shading is
/// there to separate small neighbours, so it fades out on the big ones, where
/// there is a border and a label doing the same job.
const CUSHION_FULL_AT: f64 = 44.0;

script_mod! {
    use mod.prelude.widgets_internal.*
    use mod.widgets.*

    /** One rectangle of the map: a Van Wijk cushion — a shallow pillow lit
     * from the upper left — inside a hard border. The cushion is what makes a
     * dense map readable: adjacent tiles of the same hue are separated by
     * their own shading even where there is no room for a border line. */
    set_type_default() do #(DrawMapTile::script_shader(vm)) {
        ..mod.draw.DrawQuad
        /** the tile's own colour */
        color: #x40507a
        /** the border drawn around the tile */
        edge: #x16161e
        /** cushion depth 0..1 step 0.05 */
        cushion: 0.55
        /** border thickness in points 0..3 step 0.25 */
        border: 1.0
        pixel: fn() {
            let p = self.pos * self.rect_size
            let d = min(min(p.x, p.y), min(self.rect_size.x - p.x, self.rect_size.y - p.y))
            // The pillow's surface normal. The height field is the classic
            // x(1-x)·y(1-y) parabola, so its slope is linear in the position
            // and costs two multiplies.
            let nx = self.cushion * (1.0 - 2.0 * self.pos.x)
            let ny = self.cushion * (1.0 - 2.0 * self.pos.y)
            let n = normalize(vec3(-nx, -ny, 1.0))
            let l = normalize(vec3(-0.45, -0.62, 0.64))
            let h = normalize(l + vec3(0.0, 0.0, 1.0))
            let diff = clamp(dot(n, l), 0.0, 1.0)
            let spec = pow(clamp(dot(n, h), 0.0, 1.0), /**highlight tightness 4..64 step 2*/ 26.0)
            let lit = self.color.rgb * (/**ambient 0.2..1 step 0.02*/ 0.56 + /**diffuse 0..1.5 step 0.02*/ 0.68 * diff)
                + vec3(spec, spec, spec) * /**highlight 0..0.6 step 0.02*/ 0.18
            let cov = clamp((self.border - d) * 2.0 + 0.5, 0.0, 1.0)
            let c = mix(vec4(lit, self.color.w), self.edge, cov)
            return vec4(c.rgb * c.w, c.w)
        }
    }

    mod.widgets.MpfTreemapBase = #(TreemapView::register_widget(vm))
    mod.widgets.MpfTreemap = set_type_default() do mod.widgets.MpfTreemapBase{
        width: Fill
        height: Fill
        draw_bg +: {color: mod.mpf.bg}
        draw_tile +: {}
        draw_text +: {
            color: mod.mpf.fg
            text_style: theme.font_regular{font_size: 8.0}
        }
        draw_bold +: {
            color: mod.mpf.fg_bright
            text_style: theme.font_bold{font_size: 8.5}
        }
    }
}

#[derive(Script, ScriptHook)]
#[repr(C)]
pub struct DrawMapTile {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    edge: Vec4f,
    #[live]
    cushion: f32,
    #[live]
    border: f32,
}

/// What a press on the map means to the folder view around it.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum TreemapAction {
    /// A rectangle was picked. The map keeps showing it; the browser may
    /// select it too when it happens to be in the current listing.
    Selected(PathBuf),
    /// A file was double-clicked: take the browser to where it lives.
    Reveal(PathBuf),
    /// What was picked is not on the disk any more; the map has dropped it.
    Vanished(PathBuf),
    #[default]
    None,
}

/// The kind class a file's colour comes from: the index into
/// [`Palette::kinds`]. Kept here rather than on `FileKind` because it is a
/// property of *this picture*, not of the file.
pub fn kind_class(kind: FileKind) -> u8 {
    match kind {
        FileKind::Video => 0,
        FileKind::Image => 1,
        FileKind::Audio => 2,
        FileKind::Code => 3,
        // A PDF reads as a document, which is what the text hue means here.
        FileKind::Text | FileKind::Pdf => 4,
        FileKind::Archive => 5,
        FileKind::Folder | FileKind::Generic => 6,
    }
}

/// One message from the scan worker. `generation` is the request it answers,
/// so a scan the user already navigated away from is dropped rather than
/// folded into the folder they are looking at now.
struct ScanMessage {
    generation: u64,
    step: Option<ScanStep>,
    finished: Option<Outcome>,
}

/// How a request for a folder's map ended.
enum Outcome {
    /// The disk was walked. The tree in hand is fresh, and worth saving.
    Scanned,
    /// The saved map was good and is what got delivered — nothing was read
    /// off the disk at all, which is the whole point of keeping it.
    Loaded { scanned_at: u64 },
    /// Cancelled, or the folder could not be read.
    Failed,
}

/// What the footer keeps saying after a click — held apart from the cell list
/// because a relayout throws every cell away and the selection must survive
/// it.
#[derive(Clone, Debug, PartialEq)]
struct Pick {
    path: PathBuf,
    size: u64,
    files: u32,
    is_dir: bool,
    bundle: u32,
}

/// A name waiting to be drawn on top of the finished tiles.
struct Label {
    at: DVec2,
    room: f64,
    line: String,
    below: Option<String>,
    ink: Vec4f,
}

#[derive(Script, ScriptHook, Widget)]
pub struct TreemapView {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    // The whole panel, not one of the draw calls inside it: a redraw of this
    // view has to invalidate the map, and the last thing any of the shaders
    // below touched is a strip at one edge of it.
    #[redraw]
    #[area]
    area: Area,
    #[live]
    draw_bg: DrawColor,
    #[live]
    draw_tile: DrawMapTile,
    #[live]
    draw_text: DrawText,
    #[live]
    draw_bold: DrawText,

    /// The folder the map is of — the browser's folder.
    #[rust]
    root: PathBuf,
    /// The names between the mapped folder and the one the map is zoomed
    /// into. Empty means the whole scan is on screen.
    #[rust]
    zoom: Vec<String>,
    #[rust]
    tree: Node,
    #[rust]
    style: MapStyle,
    #[rust]
    cells: Vec<Cell>,
    /// The rect `cells` was laid out for; a different one means re-layout.
    #[rust]
    laid_out: Rect,
    #[rust]
    stale: bool,
    #[rust]
    last_layout: Option<Instant>,
    #[rust]
    frame: NextFrame,

    #[rust]
    generation: u64,
    #[rust]
    cancel: Option<Arc<AtomicBool>>,
    #[rust]
    scanning: bool,
    /// Folders the walk has not opened yet. A scan cannot know its own
    /// denominator before it has walked the tree, so this is a count, not a
    /// percentage — and unlike a percentage it is true.
    #[rust]
    folders_left: u32,
    /// When the numbers on screen were measured, in seconds since the epoch.
    /// A cached map is only safe to show if it says how old it is.
    #[rust]
    scanned_at: u64,
    /// Folders the scan was refused, named so the total's shortfall is
    /// admitted rather than hidden.
    #[rust]
    denied: Vec<String>,
    /// Where "rescan" was drawn, for the click that starts one.
    #[rust]
    rescan_hit: Rect,
    #[rust]
    error: Option<String>,

    #[rust]
    sender: Option<Sender<ScanMessage>>,
    #[rust]
    receiver: Option<Receiver<ScanMessage>>,

    #[rust]
    hover: Option<usize>,
    #[rust]
    pick: Option<Pick>,
    /// Where each breadcrumb segment was drawn, and how many names of `zoom`
    /// it stands for.
    #[rust]
    crumbs: Vec<CrumbHit>,
}

/// One clickable breadcrumb segment.
#[derive(Clone, Copy)]
struct CrumbHit {
    rect: Rect,
    depth: usize,
}

impl TreemapView {
    /// The folder the map is currently of.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder the map is zoomed into — the root itself when it is not.
    pub fn focus_path(&self) -> PathBuf {
        let mut path = self.root.clone();
        for name in &self.zoom {
            path.push(name);
        }
        path
    }

    /// The node the map is drawing, following the zoom as far as it still
    /// resolves.
    fn focused(&self) -> &Node {
        let mut node = &self.tree;
        for name in &self.zoom {
            match node.child_named(name) {
                Some(next) => node = next,
                None => break,
            }
        }
        node
    }

    /// Map `path`, from the saved map when there is one. A scan already
    /// running for another folder is cancelled first — the user asked for
    /// this folder, not that one.
    pub fn set_root(&mut self, cx: &mut Cx, path: &Path) {
        // Asking for the folder already on screen is not a request to measure
        // it again. The browser re-lists its folder after every operation, and
        // the map it just corrected by arithmetic must survive that — throwing
        // it away would undo the whole point of keeping one.
        if path == self.root && !self.tree.children.is_empty() && self.error.is_none() {
            return;
        }
        self.begin(cx, path, false);
    }

    /// Measure the disk again and replace the saved map, whatever its age.
    /// The one thing that makes a cached map safe to trust: it is never more
    /// than a keystroke away from being made true.
    pub fn rescan(&mut self, cx: &mut Cx) {
        let root = self.root.clone();
        if root.as_os_str().is_empty() {
            return;
        }
        crate::sizecache::forget(&root);
        self.begin(cx, &root, true);
    }

    fn begin(&mut self, cx: &mut Cx, path: &Path, fresh: bool) {
        if self.sender.is_none() {
            let (sender, receiver) = channel();
            self.sender = Some(sender);
            self.receiver = Some(receiver);
        }
        self.stop(cx);
        self.root = path.to_path_buf();
        self.zoom.clear();
        self.tree = Node::dir(crate::model::display_name(path), FileKind::Folder as u8);
        self.cells.clear();
        self.laid_out = Rect::default();
        self.stale = true;
        self.last_layout = None;
        self.hover = None;
        self.pick = None;
        self.error = None;
        self.folders_left = 0;
        self.scanned_at = 0;
        self.scanning = true;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let root = self.root.clone();
        thread::spawn(move || {
            // The four scan threads all report through here, so the channel
            // and the signal clock live behind one lock. Waking the UI is the
            // expensive half and is what gets rate-limited; the steps
            // themselves queue as fast as the disk produces them.
            let gate = Mutex::new(Instant::now());
            let sink = |step: ScanStep| {
                if sender
                    .send(ScanMessage {
                        generation,
                        step: Some(step),
                        finished: None,
                    })
                    .is_err()
                {
                    return;
                }
                let mut due = gate.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if now >= *due {
                    *due = now + SIGNAL_EVERY;
                    SignalToUI::set_ui_signal();
                }
            };
            // The saved map first, and off the UI thread: decoding a home
            // directory's worth of tree is a tenth of a second of work that
            // has no business happening between two frames.
            let cached = if fresh || crate::vfs::is_demo() {
                None
            } else {
                crate::sizecache::load(&root)
            };
            if let Some(cached) = cached {
                let _ = sender.send(ScanMessage {
                    generation,
                    step: Some(ScanStep::Closed {
                        at: Vec::new(),
                        node: cached.tree,
                    }),
                    finished: None,
                });
                let _ = sender.send(ScanMessage {
                    generation,
                    step: None,
                    finished: Some(Outcome::Loaded {
                        scanned_at: cached.scanned_at,
                    }),
                });
                SignalToUI::set_ui_signal();
                return;
            }
            let ok = crate::vfs::vfs().scan_stream(&root, &cancel, &sink);
            let _ = sender.send(ScanMessage {
                generation,
                step: None,
                finished: Some(if ok { Outcome::Scanned } else { Outcome::Failed }),
            });
            SignalToUI::set_ui_signal();
        });
        self.redraw(cx);
    }

    /// Stop whatever scan is running. Called when the view is left, when the
    /// folder changes, and when the window goes away.
    pub fn stop(&mut self, cx: &mut Cx) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if self.scanning {
            self.scanning = false;
            self.redraw(cx);
        }
    }

    /// The status line for the map: what it is showing, or how far the scan
    /// has got, and what the last click landed on.
    pub fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let node = self.focused();
        let where_it_is = self
            .focus_path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.focus_path().display().to_string());
        if self.scanning {
            return format!(
                "Scanning {where_it_is} — {} files · {} so far · {} folder{} still open",
                self.tree.files,
                treemap::format_bytes(self.tree.size),
                self.folders_left,
                if self.folders_left == 1 { "" } else { "s" },
            );
        }
        let picked = match &self.pick {
            Some(pick) => format!(" · picked {}", crate::model::display_name(&pick.path)),
            None => String::new(),
        };
        format!(
            "{where_it_is} — {} in {} files · double-click a folder to zoom in, Esc to go back{picked}",
            treemap::format_bytes(node.size),
            node.files,
        )
    }

    /// Take everything the worker sent. True when the view needs a redraw.
    pub fn drain(&mut self, cx: &mut Cx) -> bool {
        let messages: Vec<ScanMessage> = self
            .receiver
            .as_ref()
            .map(|r| r.try_iter().collect())
            .unwrap_or_default();
        if messages.is_empty() {
            return false;
        }
        let mut finished = false;
        for message in messages {
            if message.generation != self.generation {
                continue;
            }
            if let Some(step) = message.step {
                if let ScanStep::Pace { folders_left } = &step {
                    // Cheap and constant: no tree walk, just the walk's own
                    // count of folders it has not opened yet.
                    self.folders_left = *folders_left;
                    continue;
                }
                self.tree.apply(step);
                self.stale = true;
            }
            if let Some(outcome) = message.finished {
                self.scanning = false;
                self.cancel = None;
                self.stale = true;
                self.folders_left = 0;
                finished = true;
                // Nothing is growing any more, so nothing is still pending.
                self.tree.seal();
                self.denied = self.tree.denied_paths(4);
                match outcome {
                    Outcome::Scanned => {
                        self.scanned_at = crate::sizecache::now();
                        self.save_cache();
                    }
                    Outcome::Loaded { scanned_at } => self.scanned_at = scanned_at,
                    Outcome::Failed => {
                        if self.tree.children.is_empty() {
                            self.error = Some(format!(
                                "Could not map {}",
                                crate::model::display_name(&self.root)
                            ));
                        }
                    }
                }
            }
        }
        // While the walk is running the tree changes far faster than the
        // picture needs to; a finished scan always redraws at once.
        if finished || self.layout_is_due() {
            self.redraw(cx);
        } else {
            // Nothing gets lost: the trailing update is picked up on the next
            // frame, once the throttle has expired.
            self.frame = cx.new_next_frame();
        }
        true
    }

    /// Whether the picture may be rebuilt now. The throttle exists only to
    /// keep a running scan from re-laying out the map hundreds of times a
    /// second; once nothing is feeding it any more there is nothing to
    /// throttle, and a map still showing a mid-scan snapshot after the walk
    /// has finished would be quietly, plausibly wrong.
    fn layout_is_due(&self) -> bool {
        if !self.scanning {
            return true;
        }
        match self.last_layout {
            Some(at) => at.elapsed() >= RELAYOUT_EVERY,
            None => true,
        }
    }

    /// Which path is highlighted on the map.
    pub fn set_selected(&mut self, cx: &mut Cx, path: Option<PathBuf>) {
        let same = match (&self.pick, &path) {
            (Some(pick), Some(path)) => &pick.path == path,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.pick = path.map(|path| Pick {
            path,
            size: 0,
            files: 0,
            is_dir: false,
            bundle: 0,
        });
        // The real numbers come from the cell when there is one, so a reveal
        // from the list view reads the same as a click on the map.
        if let Some(pick) = &self.pick {
            if let Some(cell) = self.cells.iter().find(|c| c.path == pick.path) {
                self.pick = Some(pick_of(cell));
            }
        }
        self.redraw(cx);
    }

    /// The path the last click landed on.
    pub fn selection(&self) -> Option<PathBuf> {
        self.pick.as_ref().map(|p| p.path.clone())
    }

    /// Zoom out one level. False when the map is already at the top.
    pub fn zoom_out(&mut self, cx: &mut Cx) -> bool {
        if self.zoom.pop().is_none() {
            return false;
        }
        self.after_zoom(cx);
        true
    }

    /// Zoom to `depth` names deep, for a breadcrumb click.
    fn zoom_to(&mut self, cx: &mut Cx, depth: usize) {
        if depth >= self.zoom.len() {
            return;
        }
        self.zoom.truncate(depth);
        self.after_zoom(cx);
    }

    /// Zoom into `path`, which must be under the mapped folder. False when it
    /// is not, or is not a folder the scan knows about.
    pub fn zoom_into(&mut self, cx: &mut Cx, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let names: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if names.is_empty() {
            return false;
        }
        let mut node = &self.tree;
        for name in &names {
            match node.child_named(name) {
                Some(next) if next.is_dir => node = next,
                _ => return false,
            }
        }
        self.zoom = names;
        self.after_zoom(cx);
        true
    }

    fn after_zoom(&mut self, cx: &mut Cx) {
        self.hover = None;
        self.stale = true;
        self.last_layout = None;
        self.laid_out = Rect::default();
        self.redraw(cx);
    }

    /// Write the finished tree out for next time. Encoding walks the whole
    /// tree so it happens here, where the tree is; the file write is somebody
    /// else's problem, on a thread nobody is waiting for.
    fn save_cache(&self) {
        if crate::vfs::is_demo() {
            return;
        }
        let Some(bytes) = crate::sizecache::encode(&self.root, &self.tree, self.scanned_at) else {
            return;
        };
        let root = self.root.clone();
        thread::spawn(move || crate::sizecache::store(&root, &bytes));
    }

    /// `path` as the chain of names between the mapped folder and it.
    fn names_of(&self, path: &Path) -> Option<Vec<String>> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let names: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!names.is_empty()).then_some(names)
    }

    /// Fold a set of finished moves into the map instead of measuring the
    /// disk again. Each pair is where something was and where it went, with
    /// `None` for "it stopped existing".
    ///
    /// This is the whole reason the map is worth caching: the app already
    /// knows exactly how big the thing it just deleted was, so the picture can
    /// be made true again by arithmetic — a delete costs no disk reads at all.
    /// A scan in flight owns the tree and will produce the truth on its own,
    /// so this stays out of its way.
    pub fn absorb_moves(&mut self, cx: &mut Cx, moves: &[(PathBuf, Option<PathBuf>)]) {
        if self.scanning || self.tree.children.is_empty() {
            return;
        }
        let mut changed = false;
        for (from, to) in moves {
            let Some(names) = self.names_of(from) else {
                continue;
            };
            let Some(mut node) = self.tree.detach(&names) else {
                continue;
            };
            changed = true;
            if self.pick.as_ref().is_some_and(|p| &p.path == from) {
                self.pick = None;
            }
            // Moved rather than removed, and landed somewhere still on the
            // map: the bytes did not leave, so neither does the rectangle.
            let Some(to) = to else { continue };
            let Some(name) = to.file_name() else { continue };
            node.name = name.to_string_lossy().into_owned();
            self.graft_at(to, node);
        }
        if changed {
            self.after_change(cx);
        }
    }

    /// Fold finished copies in: the source stays where it is and a second
    /// rectangle of the same size appears at the destination.
    pub fn absorb_copies(&mut self, cx: &mut Cx, copies: &[(PathBuf, PathBuf)]) {
        if self.scanning || self.tree.children.is_empty() {
            return;
        }
        let mut changed = false;
        for (from, to) in copies {
            let Some(names) = self.names_of(from) else {
                continue;
            };
            let Some(indices) = self.names_of(to) else {
                continue;
            };
            let Some(source) = self.tree.at(&names) else {
                continue;
            };
            let mut node = source.clone();
            let Some(name) = to.file_name() else { continue };
            node.name = name.to_string_lossy().into_owned();
            let _ = indices;
            if self.graft_at(to, node) {
                changed = true;
            }
        }
        if changed {
            self.after_change(cx);
        }
    }

    /// Put `node` where `full` says, which is a path *including* the node's
    /// own name — the parent is what actually receives it.
    fn graft_at(&mut self, full: &Path, node: Node) -> bool {
        let Some(parent) = full.parent() else {
            return false;
        };
        if parent == self.root {
            return self.tree.graft(&[], node);
        }
        match self.names_of(parent) {
            Some(names) => self.tree.graft(&names, node),
            None => false,
        }
    }

    /// Drop `path` from the map because the disk says it is not there any
    /// more. Cheap, exact, and the answer to a cached map going stale one
    /// file at a time.
    pub fn forget(&mut self, cx: &mut Cx, path: &Path) {
        if self.scanning {
            return;
        }
        let Some(names) = self.names_of(path) else {
            return;
        };
        if self.tree.detach(&names).is_some() {
            if self.pick.as_ref().is_some_and(|p| p.path == path) {
                self.pick = None;
            }
            self.after_change(cx);
        }
    }

    fn after_change(&mut self, cx: &mut Cx) {
        self.hover = None;
        self.stale = true;
        self.last_layout = None;
        self.save_cache();
        self.redraw(cx);
    }

    fn relayout(&mut self, rect: Rect) {
        let base = self.focus_path();
        let area = MapRect {
            x: rect.pos.x,
            y: rect.pos.y,
            w: rect.size.x,
            h: rect.size.y,
        };
        self.cells = treemap::layout(self.focused(), &base, area, &self.style);
        self.laid_out = rect;
        self.stale = false;
        self.last_layout = Some(Instant::now());
        // The cell list is new, so the hovered index means nothing any more.
        self.hover = None;
        // The selection is a path, not an index, so it survives — but its
        // numbers are refreshed from whatever cell now stands for it.
        if let Some(pick) = self.pick.take() {
            let refreshed = self
                .cells
                .iter()
                .find(|c| c.path == pick.path && !c.is_bundle())
                .map(pick_of);
            self.pick = refreshed.or(Some(pick));
        }
    }

    /// The cell under a window point, if any.
    fn hit_cell(&self, pos: DVec2) -> Option<usize> {
        treemap::hit(&self.cells, pos.x, pos.y)
    }

    /// The file or folder under a window point — what a right-click there is
    /// about. Never the "N smaller items" bundle, which is not a thing on
    /// disk and must never become the target of an operation.
    pub fn path_at(&self, pos: DVec2) -> Option<PathBuf> {
        self.hit_cell(pos)
            .map(|i| &self.cells[i])
            .filter(|c| !c.is_bundle())
            .map(|c| c.path.clone())
    }

    // ------------------------------------------------------------- painting

    fn tile_colors(&self, cell: &Cell, palette: &Palette) -> (Vec4f, f32) {
        let bg = Palette::vec4(&palette.bg);
        if cell.kind == KIND_BUNDLE {
            // Not a file: the sum of everything too small to see. It reads as
            // a texture rather than as a thing, which is what it is.
            return (blend(Palette::vec4(&palette.muted), bg, 0.45), 0.35);
        }
        let class = kind_class(cell_kind(cell)) as usize;
        let mut hue = palette.kind_color(class);
        if class == OTHER_CLASS && !cell.is_dir {
            // The theme's "other" is a chrome grey — the colour of a border,
            // not of a thing. A 4 GB disk image or a database file painted in
            // it disappears into the background, and on a real disk the
            // unclassifiable blobs are most of what there is to clean up.
            hue = blend(hue, Palette::vec4(&palette.fg), 0.55);
        }
        if cell.is_group {
            // A group is the plate its children sit on: nearly background, but
            // carrying a trace of its own heaviest content's hue so the shape
            // of the disk survives even where nothing inside it fits.
            let plate = blend(hue, bg, 0.86 - 0.02 * cell.depth.min(4) as f32);
            return (plate, 0.22);
        }
        // Leaves darken slightly with depth, which reads as "further in"
        // without ever making two kinds look like each other.
        let depth_shade = 1.0 - 0.05 * cell.depth.min(6) as f32;
        let base = if cell.is_dir {
            // A folder too small to open is still a folder: half way to the
            // plate, so it never reads as one big file.
            blend(hue, bg, 0.45)
        } else {
            hue
        };
        (scale_rgb(base, depth_shade), 0.62)
    }

    fn draw_map(&mut self, cx: &mut Cx2d, palette: &Palette, clip: Rect) -> Vec<Label> {
        let border_ink = Palette::vec4(&palette.bg_dark);
        let accent = Palette::vec4(&palette.accent);
        let bright = Palette::vec4(&palette.fg_bright);
        let ink_dark = Palette::vec4(&palette.bg_dark);
        let hovered = self.hover;
        let picked = self.pick.as_ref().map(|p| p.path.clone());
        let mut labels: Vec<Label> = Vec::new();

        cx.push_clip_rect(clip);
        self.draw_tile.begin_many_instances(cx);
        for (index, cell) in self.cells.iter().enumerate() {
            let rect = Rect {
                pos: dvec2(cell.rect.x, cell.rect.y),
                size: dvec2(cell.rect.w, cell.rect.h),
            };
            let (mut fill, cushion) = self.tile_colors(cell, palette);
            let is_hover = Some(index) == hovered;
            let is_pick = picked.as_deref() == Some(cell.path.as_path()) && !cell.is_bundle();
            if is_hover {
                fill = blend(bright, fill, 0.24);
            }
            // The border is what separates siblings, and it can never be
            // allowed to eat the tile it surrounds — a three-point rectangle
            // with a one-point border on every side is all border. So it
            // scales with the tile and simply stops existing on the small
            // ones, where the cushion's own shading does the separating.
            let short = cell.rect.short_side();
            let border = if is_pick || is_hover {
                1.5
            } else {
                (short * 0.14).min(1.0)
            };
            let cushion = cushion * (CUSHION_FULL_AT / short.max(4.0)).clamp(0.30, 1.0) as f32;
            self.draw_tile.color = fill;
            self.draw_tile.edge = if is_pick {
                accent
            } else if is_hover {
                bright
            } else {
                border_ink
            };
            self.draw_tile.cushion = cushion;
            self.draw_tile.border = border as f32;
            self.draw_tile.draw_abs(cx, rect);

            if labels.len() >= LABEL_BUDGET {
                continue;
            }
            if cell.is_group && cell.header > 0.0 {
                // A group's name goes in the strip it reserved for it, which
                // is the only place on a group that its children are not
                // about to be drawn over.
                labels.push(Label {
                    at: dvec2(rect.pos.x + 4.0, rect.pos.y + 1.0),
                    room: rect.size.x - 8.0,
                    line: format!("{}  {}", cell.name, treemap::format_bytes(cell.size)),
                    below: None,
                    ink: bright,
                });
            } else if !cell.is_group
                && rect.size.x >= LABEL_MIN.x
                && rect.size.y >= LABEL_MIN.y
            {
                let two_lines = rect.size.y >= LABEL_TWO_LINE_H;
                labels.push(Label {
                    at: dvec2(rect.pos.x + 4.0, rect.pos.y + 3.0),
                    room: rect.size.x - 8.0,
                    line: cell.name.clone(),
                    below: two_lines.then(|| treemap::format_bytes(cell.size)),
                    ink: ink_dark,
                });
            }
        }
        self.draw_tile.end_many_instances(cx);

        // The picked rectangle gets a ring on top of everything, because the
        // thing you are about to delete must be findable even when it is a
        // folder whose children cover it.
        if let Some(path) = &picked {
            if let Some(cell) = self.cells.iter().find(|c| &c.path == path && !c.is_bundle()) {
                self.draw_tile.color = Vec4f { x: 0.0, y: 0.0, z: 0.0, w: 0.0 };
                self.draw_tile.edge = accent;
                self.draw_tile.cushion = 0.0;
                self.draw_tile.border = 2.0;
                self.draw_tile.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(cell.rect.x, cell.rect.y),
                        size: dvec2(cell.rect.w, cell.rect.h),
                    },
                );
            }
        }
        cx.pop_clip_rect();
        labels
    }

    fn draw_labels(&mut self, cx: &mut Cx2d, labels: Vec<Label>, clip: Rect) {
        cx.push_clip_rect(clip);
        for label in labels {
            self.draw_text.color = label.ink;
            let line = fit_text(&self.draw_text, cx, &label.line, label.room);
            self.draw_text.draw_abs(cx, label.at, &line);
            if let Some(below) = label.below {
                self.draw_text.color = fade(label.ink, 0.72);
                let below = fit_text(&self.draw_text, cx, &below, label.room);
                self.draw_text
                    .draw_abs(cx, label.at + dvec2(0.0, 11.0), &below);
            }
        }
        cx.pop_clip_rect();
    }

    /// The zoom breadcrumb, and on the right whatever the scan is doing.
    fn draw_crumbs(&mut self, cx: &mut Cx2d, strip: Rect, palette: &Palette) {
        self.crumbs.clear();
        self.draw_bg.color = Palette::vec4(&palette.bg_dark);
        self.draw_bg.draw_abs(cx, strip);

        let bright = Palette::vec4(&palette.fg_bright);
        let dim = Palette::vec4(&palette.fg_dim);
        let accent = Palette::vec4(&palette.accent);

        // The right-hand end first, so the crumbs know where to stop. This is
        // where the map admits what it is: how old the numbers are, what it
        // was not allowed to look at, and what it left out on purpose.
        self.rescan_hit = Rect::default();
        let mut right = strip.pos.x + strip.size.x - 8.0;
        if !self.scanning {
            // "Rescan" is not a nicety. A map read back from a file is only
            // honest if making it true again is one click away.
            // A word, not a glyph: the UI font has no reload arrow and a
            // tofu box is worse than no icon at all. Icons in this app are
            // SVGs, and this control does not need one.
            let word = "rescan";
            let width = text_width(&self.draw_bold, cx, word);
            right -= width;
            self.draw_bold.color = accent;
            self.draw_bold.draw_abs(cx, dvec2(right, strip.pos.y + 4.0), word);
            self.rescan_hit = Rect {
                pos: dvec2(right - 4.0, strip.pos.y),
                size: dvec2(width + 8.0, strip.size.y),
            };
            right -= 14.0;
        }
        let note = if self.scanning {
            format!(
                "scanning  ·  {} files  ·  {}  ·  {} folders open",
                self.tree.files,
                treemap::format_bytes(self.tree.size),
                self.folders_left,
            )
        } else {
            let mut note = crate::sizecache::age_text(self.scanned_at);
            if let Some(excluded) = crate::model::scan_exclusions() {
                note.push_str("  ·  ");
                note.push_str(&excluded);
            }
            if !self.denied.is_empty() {
                note.push_str("  ·  no access: ");
                note.push_str(&self.denied.join(", "));
            }
            note
        };
        let note_w = text_width(&self.draw_text, cx, &note);
        self.draw_text.color = if self.scanning { accent } else { dim };
        self.draw_text
            .draw_abs(cx, dvec2(right - note_w, strip.pos.y + 5.0), &note);
        let note_w = strip.pos.x + strip.size.x - (right - note_w);

        let limit = strip.pos.x + strip.size.x - note_w - 18.0;
        let mut x = strip.pos.x + 8.0;
        let names: Vec<String> = std::iter::once(crate::model::display_name(&self.root))
            .chain(self.zoom.iter().cloned())
            .collect();
        let last = names.len().saturating_sub(1);
        for (depth, name) in names.iter().enumerate() {
            let text = if depth == 0 {
                name.clone()
            } else {
                format!("› {name}")
            };
            let width = text_width(&self.draw_bold, cx, &text);
            if x + width > limit {
                self.draw_text.color = dim;
                self.draw_text.draw_abs(cx, dvec2(x, strip.pos.y + 5.0), "…");
                break;
            }
            self.draw_bold.color = if depth == last { bright } else { dim };
            self.draw_bold.draw_abs(cx, dvec2(x, strip.pos.y + 4.0), &text);
            self.crumbs.push(CrumbHit {
                rect: Rect {
                    pos: dvec2(x, strip.pos.y),
                    size: dvec2(width, strip.size.y),
                },
                depth,
            });
            x += width + 6.0;
        }
    }

    /// The persistent readout: what the last click landed on, and how big it
    /// is. This is the line a person cleaning up a full disk actually reads,
    /// so it never goes away on its own and never shows anything but the
    /// truth about one real path.
    fn draw_footer(&mut self, cx: &mut Cx2d, strip: Rect, palette: &Palette) {
        self.draw_bg.color = Palette::vec4(&palette.bg_dark);
        self.draw_bg.draw_abs(cx, strip);
        let total = self.focused().size.max(1);
        // Two parts: the numbers, then the path. The numbers are the reason
        // anybody is looking and always get their room first; the path takes
        // whatever is left and is shortened from its *front*, because the end
        // of a path is the half that says which file this is.
        let (head, path, ink) = match &self.pick {
            Some(pick) if pick.bundle > 0 => (
                format!(
                    "{}   {:.1}% of what is on screen",
                    treemap::format_bytes(pick.size),
                    pick.size as f64 * 100.0 / total as f64,
                ),
                format!(
                    "{} items each too small to draw on their own",
                    pick.bundle
                ),
                Palette::vec4(&palette.fg_dim),
            ),
            Some(pick) => (
                format!(
                    "{}   {:.1}% of {}{}",
                    treemap::format_bytes(pick.size),
                    pick.size as f64 * 100.0 / total as f64,
                    treemap::format_bytes(total),
                    if pick.is_dir {
                        format!("   {} files", pick.files)
                    } else {
                        String::new()
                    },
                ),
                pick.path.display().to_string(),
                Palette::vec4(&palette.fg_bright),
            ),
            None => (
                String::new(),
                "Click a rectangle to pick it · double-click a folder to zoom in · Esc goes back \
                 · right-click for the file menu"
                    .to_string(),
                Palette::vec4(&palette.fg_dim),
            ),
        };
        let baseline = strip.pos.y + 5.0;
        let mut x = strip.pos.x + 8.0;
        let edge = strip.pos.x + strip.size.x - 8.0;
        if !head.is_empty() {
            self.draw_text.color = ink;
            let head = fit_text(&self.draw_text, cx, &head, edge - x);
            let width = text_width(&self.draw_text, cx, &head);
            self.draw_text.draw_abs(cx, dvec2(x, baseline), &head);
            x += width + 12.0;
        }
        self.draw_text.color = Palette::vec4(&palette.fg_dim);
        let path = fit_tail(&self.draw_text, cx, &path, edge - x);
        self.draw_text.draw_abs(cx, dvec2(x, baseline), &path);
    }

    /// The hovered rectangle's name and size, anchored to the rectangle
    /// itself rather than to the pointer — a tooltip that chases the mouse
    /// forces a full repaint of the whole map on every mouse move, and a map
    /// can be sixty thousand rectangles.
    fn draw_tooltip(&mut self, cx: &mut Cx2d, body: Rect, palette: &Palette) {
        let Some(cell) = self.hover.and_then(|i| self.cells.get(i)) else {
            return;
        };
        let total = self.focused().size.max(1);
        let head = if cell.is_bundle() {
            cell.name.clone()
        } else {
            cell.path
                .strip_prefix(&self.focus_path())
                .unwrap_or(&cell.path)
                .display()
                .to_string()
        };
        let foot = format!(
            "{} · {:.1}%{}{}",
            treemap::format_bytes(cell.size),
            cell.size as f64 * 100.0 / total as f64,
            if cell.is_dir {
                format!(" · {} files", cell.files)
            } else {
                String::new()
            },
            if cell.pending { " · still scanning" } else { "" },
        );
        let anchor = dvec2(cell.rect.x, cell.rect.y);
        let cell_h = cell.rect.h;

        let width = text_width(&self.draw_bold, cx, &head)
            .max(text_width(&self.draw_text, cx, &foot))
            + 14.0;
        let size = dvec2(width.min(body.size.x - 8.0), 30.0);
        // Below the rectangle when there is room under it, above it when
        // there is not — so the tooltip never covers what it is describing.
        let below = anchor.y + cell_h + 4.0;
        let y = if below + size.y <= body.pos.y + body.size.y {
            below
        } else {
            (anchor.y - size.y - 4.0).max(body.pos.y + 2.0)
        };
        let pos = dvec2(
            anchor
                .x
                .min(body.pos.x + body.size.x - size.x - 4.0)
                .max(body.pos.x + 2.0),
            y,
        );

        // Both the plate and its text open a draw call of their own. Every
        // tile shares one batch and every label shares another, so without
        // this the plate lands in the batch the map was drawn in and the
        // labels underneath read straight through it.
        self.draw_tile.new_draw_call(cx);
        self.draw_tile.color = Palette::vec4(&palette.bg_dark);
        self.draw_tile.edge = Palette::vec4(&palette.accent);
        self.draw_tile.cushion = 0.0;
        self.draw_tile.border = 1.0;
        self.draw_tile.draw_abs(cx, Rect { pos, size });

        cx.push_clip_rect(Rect { pos, size });
        self.draw_bold.new_draw_call(cx);
        self.draw_bold.color = Palette::vec4(&palette.fg_bright);
        self.draw_bold.draw_abs(cx, pos + dvec2(7.0, 3.0), &head);
        self.draw_text.new_draw_call(cx);
        self.draw_text.color = Palette::vec4(&palette.fg_dim);
        self.draw_text.draw_abs(cx, pos + dvec2(7.0, 16.0), &foot);
        cx.pop_clip_rect();
    }

    fn press(&mut self, cx: &mut Cx, at: DVec2, taps: u32, primary: bool) {
        if self.rescan_hit.contains(at) {
            if primary {
                self.rescan(cx);
            }
            return;
        }
        if let Some(crumb) = self.crumbs.iter().find(|c| c.rect.contains(at)).copied() {
            if primary {
                self.zoom_to(cx, crumb.depth);
            }
            return;
        }
        let Some(index) = self.hit_cell(at) else {
            return;
        };
        let cell = self.cells[index].clone();
        // One stat, on the one thing that was just pointed at. A map read
        // back from a file can be out of date; this is where that stops being
        // invisible, and it costs nothing because it happens once per click
        // rather than once per rectangle.
        if !cell.is_bundle() && !crate::vfs::is_demo() && !crate::vfs::vfs().exists(&cell.path) {
            let path = cell.path.clone();
            self.forget(cx, &path);
            cx.widget_action(self.uid, TreemapAction::Vanished(path));
            return;
        }
        self.pick = Some(pick_of(&cell));
        self.redraw(cx);
        if cell.is_bundle() {
            // Nothing on disk is under there to act on; the footer says what
            // it stands for and that is the whole of it.
            return;
        }
        // A secondary press only picks: the context menu that follows it acts
        // on whatever is picked, and zooming out from under a menu that is
        // about to open would be a trap.
        if !primary {
            cx.widget_action(self.uid, TreemapAction::Selected(cell.path));
            return;
        }
        if taps >= 2 {
            if cell.is_dir {
                // Zooming re-roots the picture without re-reading the disk:
                // the subtree is already in hand.
                if self.zoom_into(cx, &cell.path.clone()) {
                    return;
                }
            } else {
                cx.widget_action(self.uid, TreemapAction::Reveal(cell.path));
                return;
            }
        }
        cx.widget_action(self.uid, TreemapAction::Selected(cell.path));
    }
}

impl Widget for TreemapView {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let rect = cx.walk_turtle(walk);
        let palette = Palette::shared();
        self.draw_bg.color = Palette::vec4(&palette.bg);
        self.draw_bg.draw_abs(cx, rect);
        if rect.size.x < 40.0 || rect.size.y < CRUMB_H + FOOT_H + 20.0 {
            cx.add_aligned_rect_area(&mut self.area, rect);
            return DrawStep::done();
        }

        let crumb_strip = Rect {
            pos: rect.pos,
            size: dvec2(rect.size.x, CRUMB_H),
        };
        let foot_strip = Rect {
            pos: dvec2(rect.pos.x, rect.pos.y + rect.size.y - FOOT_H),
            size: dvec2(rect.size.x, FOOT_H),
        };
        let body = Rect {
            pos: dvec2(rect.pos.x + 1.0, rect.pos.y + CRUMB_H + 1.0),
            size: dvec2(
                rect.size.x - 2.0,
                rect.size.y - CRUMB_H - FOOT_H - 2.0,
            ),
        };

        self.draw_crumbs(cx, crumb_strip, palette);
        self.draw_footer(cx, foot_strip, palette);

        if self.tree.children.is_empty() {
            self.draw_text.color = Palette::vec4(&palette.fg_dim);
            let text = self.error.clone().unwrap_or_else(|| {
                if self.scanning {
                    "Reading the folder…".to_string()
                } else {
                    "Nothing to map".to_string()
                }
            });
            self.draw_text
                .draw_abs(cx, body.pos + dvec2(16.0, 16.0), &text);
            cx.add_aligned_rect_area(&mut self.area, rect);
            return DrawStep::done();
        }

        if self.laid_out != body || (self.stale && self.layout_is_due()) {
            self.relayout(body);
        } else if self.stale {
            // Drawn from a picture the scan has already moved past. The
            // throttle says not yet, so come back for it — a skipped
            // relayout that nothing ever comes back for is a map that stops
            // updating and never says so.
            self.frame = cx.new_next_frame();
        }

        let labels = self.draw_map(cx, palette, body);
        self.draw_labels(cx, labels, body);
        self.draw_tooltip(cx, body, palette);

        cx.add_aligned_rect_area(&mut self.area, rect);
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        if self.frame.is_event(event).is_some() && self.stale {
            if self.layout_is_due() {
                self.redraw(cx);
            } else {
                self.frame = cx.new_next_frame();
            }
        }
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                cx.set_cursor(MouseCursor::Arrow);
                let hover = self.hit_cell(e.abs);
                // Only when the rectangle under the pointer actually changes:
                // the tooltip is anchored to the cell, not to the pointer, so
                // moving inside one cell costs nothing.
                if hover != self.hover {
                    self.hover = hover;
                    self.redraw(cx);
                }
            }
            Hit::FingerHoverOut(_) => {
                if self.hover.take().is_some() {
                    self.redraw(cx);
                }
            }
            Hit::FingerDown(e) => {
                let primary = e.device.is_primary_hit() && !e.modifiers.control;
                self.press(cx, e.abs, e.tap_count, primary);
            }
            _ => {}
        }
    }
}

impl TreemapViewRef {
    pub fn set_root(&self, cx: &mut Cx, path: &Path) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_root(cx, path);
        }
    }

    pub fn stop(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.stop(cx);
        }
    }

    pub fn drain(&self, cx: &mut Cx) -> bool {
        self.borrow_mut().map(|mut i| i.drain(cx)).unwrap_or(false)
    }

    pub fn status(&self) -> String {
        self.borrow().map(|i| i.status()).unwrap_or_default()
    }

    pub fn root(&self) -> PathBuf {
        self.borrow().map(|i| i.root().to_path_buf()).unwrap_or_default()
    }

    pub fn set_selected(&self, cx: &mut Cx, path: Option<PathBuf>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_selected(cx, path);
        }
    }

    pub fn selection(&self) -> Option<PathBuf> {
        self.borrow().and_then(|i| i.selection())
    }

    pub fn path_at(&self, pos: DVec2) -> Option<PathBuf> {
        self.borrow().and_then(|i| i.path_at(pos))
    }

    /// Measure the disk again, replacing whatever was cached.
    pub fn rescan(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.rescan(cx);
        }
    }

    /// Fold finished moves and deletes into the map rather than rescanning.
    pub fn absorb_moves(&self, cx: &mut Cx, moves: &[(PathBuf, Option<PathBuf>)]) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.absorb_moves(cx, moves);
        }
    }

    /// Fold finished copies into the map rather than rescanning.
    pub fn absorb_copies(&self, cx: &mut Cx, copies: &[(PathBuf, PathBuf)]) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.absorb_copies(cx, copies);
        }
    }

    /// Step back out one zoom level. False when there was nowhere to go.
    pub fn zoom_out(&self, cx: &mut Cx) -> bool {
        self.borrow_mut().map(|mut i| i.zoom_out(cx)).unwrap_or(false)
    }

    /// Zoom into whatever the last click picked, when that was a folder.
    pub fn zoom_into_selection(&self, cx: &mut Cx) -> bool {
        let Some(mut inner) = self.borrow_mut() else {
            return false;
        };
        let Some(path) = inner.selection() else {
            return false;
        };
        inner.zoom_into(cx, &path)
    }

    /// The map's action out of an event batch.
    pub fn action(&self, actions: &Actions) -> TreemapAction {
        let uid = self.widget_uid();
        actions
            .iter()
            .filter_map(|a| a.as_widget_action().filter(|wa| wa.widget_uid == uid))
            .map(|wa| wa.cast::<TreemapAction>())
            .find(|a| *a != TreemapAction::None)
            .unwrap_or(TreemapAction::None)
    }
}

fn pick_of(cell: &Cell) -> Pick {
    Pick {
        path: cell.path.clone(),
        size: cell.size,
        files: cell.files,
        is_dir: cell.is_dir,
        bundle: cell.extra,
    }
}

/// `a` over `b` at `t`, in premultiplication-free straight colour.
fn blend(a: Vec4f, b: Vec4f, t: f32) -> Vec4f {
    Vec4f {
        x: a.x * (1.0 - t) + b.x * t,
        y: a.y * (1.0 - t) + b.y * t,
        z: a.z * (1.0 - t) + b.z * t,
        w: 1.0,
    }
}

fn scale_rgb(c: Vec4f, k: f32) -> Vec4f {
    Vec4f {
        x: c.x * k,
        y: c.y * k,
        z: c.z * k,
        w: c.w,
    }
}

fn fade(c: Vec4f, k: f32) -> Vec4f {
    Vec4f { w: c.w * k, ..c }
}

/// The width one line of text would take, in points.
fn text_width(draw: &DrawText, cx: &mut Cx2d, text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let laid = draw.layout(cx, 0.0, 0.0, None, false, Align::default(), text);
    laid.rows
        .first()
        .map(|r| r.width_in_lpxs as f64)
        .unwrap_or(0.0)
}

/// `text`, shortened with an ellipsis until it fits in `room` points. The
/// first guess comes from the measured width, so the loop almost never runs
/// more than once — measuring is cached per string, but a treemap draws
/// hundreds of labels a frame and every one of them has to be cheap.
fn fit_text(draw: &DrawText, cx: &mut Cx2d, text: &str, room: f64) -> String {
    if room <= 6.0 {
        return String::new();
    }
    let full = text_width(draw, cx, text);
    if full <= room {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = ((room / full) * chars.len() as f64) as usize;
    for _ in 0..4 {
        keep = keep.min(chars.len().saturating_sub(1));
        if keep == 0 {
            return "…".to_string();
        }
        let candidate: String = chars[..keep].iter().collect::<String>() + "…";
        if text_width(draw, cx, &candidate) <= room {
            return candidate;
        }
        keep = keep * 4 / 5;
    }
    "…".to_string()
}

/// `text`, shortened from its *front* until it fits in `room` points. For a
/// path that is the right end to cut: `…/Sim/Devices/device-a/data.img` still
/// says which file this is, and `/private/tmp/claude-501/-Users-…` says
/// nothing at all.
fn fit_tail(draw: &DrawText, cx: &mut Cx2d, text: &str, room: f64) -> String {
    if room <= 6.0 {
        return String::new();
    }
    let full = text_width(draw, cx, text);
    if full <= room {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = ((room / full) * chars.len() as f64) as usize;
    for _ in 0..4 {
        keep = keep.min(chars.len().saturating_sub(1));
        if keep == 0 {
            return "…".to_string();
        }
        let candidate: String =
            String::from("…") + &chars[chars.len() - keep..].iter().collect::<String>();
        if text_width(draw, cx, &candidate) <= room {
            return candidate;
        }
        keep = keep * 4 / 5;
    }
    "…".to_string()
}

/// [`FileKind`] in discriminant order, so the opaque `u8` the scan carried —
/// which for this app is a `FileKind` discriminant — can be read back. A
/// change to the enum's order breaks this, which is what the test below is
/// for.
const FILE_KINDS: [FileKind; 9] = [
    FileKind::Folder,
    FileKind::Image,
    FileKind::Text,
    FileKind::Code,
    FileKind::Audio,
    FileKind::Video,
    FileKind::Archive,
    FileKind::Pdf,
    FileKind::Generic,
];

/// The kind a cell paints as.
fn cell_kind(cell: &Cell) -> FileKind {
    FILE_KINDS
        .get(cell.kind as usize)
        .copied()
        .unwrap_or(FileKind::Generic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_tag_survives_the_round_trip_through_a_byte() {
        // The scan stores a FileKind as its discriminant; FILE_KINDS turns it
        // back. If the enum ever gains a variant in the middle, this fails
        // before the map starts painting videos as archives.
        for (index, kind) in FILE_KINDS.iter().enumerate() {
            assert_eq!(*kind as usize, index, "{kind:?}");
        }
    }

    #[test]
    fn every_kind_lands_on_a_palette_class() {
        let palette = Palette::tokyo_night();
        for kind in FILE_KINDS {
            let class = kind_class(kind) as usize;
            assert!(class < palette.kinds.len(), "{kind:?} -> {class}");
        }
        // The classes that carry the picture are all different colors.
        let video = kind_class(FileKind::Video);
        let image = kind_class(FileKind::Image);
        let archive = kind_class(FileKind::Archive);
        assert_ne!(video, image);
        assert_ne!(image, archive);
    }

    // The bundle rectangle carries a kind no palette class answers to, and it
    // must fall through to "other" rather than index off the end.
    #[test]
    fn the_bundle_tag_is_not_a_file_kind() {
        assert!(KIND_BUNDLE as usize >= FILE_KINDS.len());
        let palette = Palette::tokyo_night();
        let class = kind_class(FileKind::Generic) as usize;
        assert!(class < palette.kinds.len());
    }
}
