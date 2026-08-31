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
    collections::HashMap,
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
    treemap::{self, Cell, MapStyle, Node, Query, Rect as MapRect, ScanStep},
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
/// How long a filter change morphs the map from the old cell set to the new.
const TWEEN: Duration = Duration::from_millis(200);
/// Points of elevation one nesting level is worth at camera scale 1 — the
/// whole meaning of the raised projections: height is depth. Big enough
/// that a nested plate clears its parent's label line.
const RISE: f64 = 11.0;
/// The perspective eye's height over the base plane, in the same points.
/// Large on purpose: the 3d mode is the ortho map breathing, not a flyover.
const PERSP_EYE: f64 = 1500.0;
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
    /// What was picked is not on the disk any more; the map has dropped it.
    Vanished(PathBuf),
    /// The ✕ on the filter chip: the map is unfiltered again, and whoever
    /// owns the filter controls should show them cleared.
    FilterCleared,
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

    /// The visual camera over the map: 1.0 shows the whole focused folder
    /// fitted to the panel; larger blows it up that many times, with
    /// `cam_off` saying how far the window has slid into the blown-up map
    /// (in points, from its top-left). Purely a way of *looking* — the
    /// breadcrumb, the browser and the scan never move with it.
    #[rust]
    cam_scale: f64,
    #[rust]
    cam_off: DVec2,
    /// A primary press that may become a pan: where it went down, where the
    /// camera was when it did, and the tap count it arrived with. The click
    /// itself is decided on release — a press that moved is a pan and picks
    /// nothing, so dragging across the map never changes the selection.
    #[rust]
    drag: Option<Drag>,
    #[rust]
    panning: bool,

    /// How the map is drawn: flat, extruded, or in perspective.
    #[rust]
    projection: MapProjection,
    /// The order cells paint in for the raised projections. Empty for the
    /// flat map, whose own vector is already painter's order.
    #[rust]
    paint_order: Vec<usize>,

    /// The live filter. None (or an empty query) is the whole disk.
    #[rust]
    filter: Option<Query>,
    /// What the filter matched under the focused folder: (bytes, files).
    #[rust]
    filtered: Option<(u64, u32)>,
    /// Where the filter chip's ✕ was drawn, for the click that clears it.
    #[rust]
    filter_hit: Rect,
    /// Byte totals per kind tag — the legend's numbers, recomputed lazily.
    #[rust]
    totals: [u64; 16],
    #[rust]
    totals_dirty: bool,

    /// The filter tween: where each surviving path was, the cells that are
    /// leaving (with the rect they were last seen at), and when it started.
    #[rust]
    tween_from: HashMap<PathBuf, TweenFrom>,
    #[rust]
    tween_leavers: Vec<(Cell, MapRect, f64)>,
    #[rust]
    tween_start: Option<Instant>,
    /// A snapshot of the map as it looks right now, taken when the filter
    /// changes, consumed by the next relayout to aim the tween.
    #[rust]
    tween_capture: Option<Vec<(Cell, MapRect, f64)>>,

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

/// A primary press waiting to learn whether it is a click or a pan.
#[derive(Clone, Copy)]
struct Drag {
    from: DVec2,
    cam_off: DVec2,
    taps: u32,
}

/// How the map is projected onto the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MapProjection {
    /// The flat map — exactly the 2D treemap.
    #[default]
    Flat,
    /// 2.5D: every cell extrudes straight up by its nesting depth, showing a
    /// darker riser below its plate. Deep tangles read as towers.
    Ortho,
    /// The same prisms through a gentle straight-down perspective: higher
    /// plates swell and lean away from the middle of the panel.
    Persp,
}

/// Where a cell was when a filter tween started, so it can glide to where it
/// is now.
struct TweenFrom {
    rect: MapRect,
    depth: f64,
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

    /// Re-open the current root under whatever the scan rules now say —
    /// the scope checkbox's move. The saved map for the *new* scope is
    /// welcome (that is what makes flipping back instant); the tree in hand
    /// was measured under the old rules and is not.
    pub fn remap(&mut self, cx: &mut Cx) {
        let root = self.root.clone();
        if root.as_os_str().is_empty() {
            return;
        }
        self.begin(cx, &root, false);
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
        // Re-measuring the folder already on screen is not a reason to lose
        // what the user had picked: the selection is a path, and the path is
        // as true after the rescan as before it. A different folder is a
        // different picture, and there the old pick would be a lie.
        let keep_pick = if path == self.root { self.pick.take() } else { None };
        self.root = path.to_path_buf();
        self.zoom.clear();
        self.tree = Node::dir(crate::model::display_name(path), FileKind::Folder as u8);
        self.cells.clear();
        self.laid_out = Rect::default();
        self.stale = true;
        self.last_layout = None;
        self.hover = None;
        self.pick = keep_pick;
        self.error = None;
        self.folders_left = 0;
        self.scanned_at = 0;
        self.scanning = true;
        self.cam_scale = 1.0;
        self.cam_off = DVec2::default();
        self.drag = None;
        self.panning = false;
        self.filtered = None;
        self.totals_dirty = true;
        self.tween_capture = None;
        self.tween_start = None;
        self.tween_from.clear();
        self.tween_leavers.clear();

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
            "{where_it_is} — {} in {} files · scroll zooms, drag pans, Esc backs out{picked}",
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
                self.totals_dirty = true;
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

    /// Step the view back out. The camera first — Esc un-zooms what the eye
    /// did before it re-roots what a reveal did. False when there is nowhere
    /// left to go.
    pub fn zoom_out(&mut self, cx: &mut Cx) -> bool {
        if self.cam_scale > 1.001 {
            self.set_camera(cx, 1.0, DVec2::default());
            return true;
        }
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
        // A re-root is a new picture; the camera starts over on it.
        self.cam_scale = 1.0;
        self.cam_off = DVec2::default();
        self.redraw(cx);
    }

    // ------------------------------------------------------------- camera

    /// Move the camera: clamp so the window never leaves the map, then lay
    /// the map out again at the new magnification. The re-layout is the whole
    /// point of zooming here — the magnified map is laid out at its blown-up
    /// size and culled to the window, so detail that was below the
    /// visibility floor comes into existence instead of scaling up blurry,
    /// and "N smaller items" plates dissolve into the things they stood for.
    fn set_camera(&mut self, cx: &mut Cx, scale: f64, off: DVec2) {
        let body = self.laid_out;
        let scale = scale.clamp(1.0, 512.0);
        let off = dvec2(
            off.x.clamp(0.0, (body.size.x * (scale - 1.0)).max(0.0)),
            off.y.clamp(0.0, (body.size.y * (scale - 1.0)).max(0.0)),
        );
        if (scale - self.cam_scale).abs() < 1e-9 && (off - self.cam_off).length() < 1e-9 {
            return;
        }
        self.cam_scale = scale;
        self.cam_off = off;
        self.stale = true;
        self.last_layout = None;
        self.hover = None;
        self.redraw(cx);
    }

    /// Zoom by `factor`, keeping the map point under `at` exactly where it
    /// is — the anchor rule every map application follows.
    fn zoom_at(&mut self, cx: &mut Cx, at: DVec2, factor: f64) {
        let body = self.laid_out;
        if body.size.x <= 0.0 || body.size.y <= 0.0 {
            return;
        }
        let old = self.cam_scale.max(1.0);
        let new = (old * factor).clamp(1.0, 512.0);
        let factor = new / old;
        let anchor = at - body.pos;
        self.set_camera(
            cx,
            new,
            dvec2(
                (self.cam_off.x + anchor.x) * factor - anchor.x,
                (self.cam_off.y + anchor.y) * factor - anchor.y,
            ),
        );
    }

    // -------------------------------------------------- projection & filter

    /// One nesting level's worth of elevation, in on-screen points. Grows
    /// with the square root of the camera so towers stay proud when zoomed
    /// without ever dwarfing the tiles.
    fn rise(&self) -> f64 {
        RISE * self.cam_scale.max(1.0).sqrt()
    }

    /// The elevation of a plate at `depth`. The top level sits on the floor
    /// — exactly where the flat map has it — and every nesting level steps
    /// up one rise from there.
    fn elev(&self, depth: usize) -> f64 {
        depth.min(24) as f64 * self.rise()
    }

    fn elev_f(&self, depth: f64) -> f64 {
        depth.min(24.0) * self.rise()
    }

    /// `rect` as drawn at elevation `z` under the current projection.
    fn project_rect(&self, rect: &MapRect, z: f64) -> MapRect {
        match self.projection {
            MapProjection::Flat => *rect,
            MapProjection::Ortho => MapRect {
                x: rect.x,
                y: rect.y - z,
                w: rect.w,
                h: rect.h,
            },
            MapProjection::Persp => {
                let body = self.laid_out;
                let cx = body.pos.x + body.size.x * 0.5;
                let cy = body.pos.y + body.size.y * 0.5;
                let s = (PERSP_EYE / (PERSP_EYE - z)).clamp(1.0, 1.6);
                MapRect {
                    x: cx + (rect.x - cx) * s,
                    y: cy + (rect.y - cy) * s,
                    w: rect.w * s,
                    h: rect.h * s,
                }
            }
        }
    }

    /// Change how the map projects. The layout itself never changes — only
    /// what is done with it on the way to the screen.
    pub fn set_projection(&mut self, cx: &mut Cx, projection: MapProjection) {
        if self.projection == projection {
            return;
        }
        self.projection = projection;
        self.hover = None;
        self.stale = true;
        self.last_layout = None;
        self.redraw(cx);
    }

    /// Apply (or clear) the live filter, morphing from the picture on screen.
    pub fn set_filter(&mut self, cx: &mut Cx, filter: Option<Query>) {
        let filter = filter.filter(|q| !q.is_empty());
        if filter == self.filter {
            return;
        }
        // Aim the tween from wherever things visually are right now — a
        // slider mid-drag retargets smoothly instead of jumping.
        self.tween_capture = Some(self.visual_snapshot());
        self.filter = filter;
        self.stale = true;
        self.last_layout = None;
        self.hover = None;
        self.redraw(cx);
    }

    /// Whether a filter is active, and what it matched: (bytes, files).
    pub fn filter_matched(&self) -> Option<(u64, u32)> {
        self.filter.as_ref()?;
        self.filtered
    }

    /// Byte totals per kind tag under the mapped folder — the legend's
    /// numbers. Recounted only after the tree actually changed.
    pub fn kind_totals(&mut self) -> [u64; 16] {
        if self.totals_dirty {
            self.totals = treemap::kind_totals(&self.tree);
            self.totals_dirty = false;
        }
        self.totals
    }

    /// Eased tween progress, or None when nothing is morphing.
    fn tween_t(&self) -> Option<f64> {
        let start = self.tween_start?;
        let t = start.elapsed().as_secs_f64() / TWEEN.as_secs_f64();
        if t >= 1.0 {
            return None;
        }
        // Smoothstep: no snap at either end.
        Some(t * t * (3.0 - 2.0 * t))
    }

    /// Every cell's current on-screen truth — layout rect and fractional
    /// depth, mid-tween or not — plus the leavers still fading out.
    fn visual_snapshot(&self) -> Vec<(Cell, MapRect, f64)> {
        let t = self.tween_t();
        let mut out: Vec<(Cell, MapRect, f64)> = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            let (rect, depth, alive) = self.tweened(cell, t);
            if alive > 0.0 {
                out.push((cell.clone(), rect, depth));
            }
        }
        if let Some(t) = t {
            for (cell, rect, _) in &self.tween_leavers {
                if 1.0 - t > 0.05 {
                    out.push((cell.clone(), *rect, cell.depth as f64));
                }
            }
        }
        out
    }

    /// Where `cell` is right now: (layout rect, fractional depth, alpha).
    fn tweened(&self, cell: &Cell, t: Option<f64>) -> (MapRect, f64, f64) {
        let Some(t) = t else {
            return (cell.rect, cell.depth as f64, 1.0);
        };
        match self.tween_from.get(&cell.path) {
            Some(from) => (
                lerp_rect(&from.rect, &cell.rect, t),
                from.depth + (cell.depth as f64 - from.depth) * t,
                1.0,
            ),
            None => {
                // An arriver: grows out of its own footprint.
                let grown = 0.7 + 0.3 * t;
                let rect = MapRect {
                    x: cell.rect.x + cell.rect.w * (1.0 - grown) * 0.5,
                    y: cell.rect.y + cell.rect.h * (1.0 - grown) * 0.5,
                    w: cell.rect.w * grown,
                    h: cell.rect.h * grown,
                };
                (rect, cell.depth as f64, t)
            }
        }
    }

    /// Fill the panel with `rect` — what a double-click means: go look at
    /// this one, without re-rooting anything.
    fn fit_rect(&mut self, cx: &mut Cx, rect: Rect) {
        let body = self.laid_out;
        if rect.size.x <= 1.0 || rect.size.y <= 1.0 || body.size.x <= 0.0 {
            return;
        }
        let old = self.cam_scale.max(1.0);
        let fit = (body.size.x / rect.size.x).min(body.size.y / rect.size.y) * 0.94;
        let new = (old * fit).clamp(1.0, 512.0);
        let factor = new / old;
        let pos = dvec2(
            (rect.pos.x - body.pos.x + self.cam_off.x) * factor,
            (rect.pos.y - body.pos.y + self.cam_off.y) * factor,
        );
        let size = dvec2(rect.size.x * factor, rect.size.y * factor);
        self.set_camera(
            cx,
            new,
            dvec2(
                pos.x - (body.size.x - size.x) * 0.5,
                pos.y - (body.size.y - size.y) * 0.5,
            ),
        );
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
        self.totals_dirty = true;
        self.last_layout = None;
        self.save_cache();
        self.redraw(cx);
    }

    fn relayout(&mut self, rect: Rect) {
        let base = self.focus_path();
        // The map is laid out at the camera's magnification and culled to
        // the panel: zoomed in, the layout does the work of the pixels on
        // screen, not of the whole magnified picture.
        let scale = self.cam_scale.max(1.0);
        let area = MapRect {
            x: rect.pos.x - self.cam_off.x,
            y: rect.pos.y - self.cam_off.y,
            w: rect.size.x * scale,
            h: rect.size.y * scale,
        };
        let viewport = MapRect {
            x: rect.pos.x,
            y: rect.pos.y,
            w: rect.size.x,
            h: rect.size.y,
        };
        // The raised projections lift plates up the screen, so give the
        // layout a little extra world below the window — otherwise a tower
        // whose footprint sits just south of the panel could never lean in.
        let viewport = match self.projection {
            MapProjection::Flat => viewport,
            _ => MapRect {
                h: viewport.h + self.elev(24),
                ..viewport
            },
        };
        self.cells = treemap::layout(
            self.focused(),
            &base,
            area,
            viewport,
            &self.style,
            self.filter.as_ref(),
        );
        self.filtered = self.filter.as_ref().map(|query| {
            let focused = self.focused();
            treemap::filtered_size(focused, query, query.name_hits(&focused.name))
        });
        self.paint_order = match self.projection {
            MapProjection::Flat => Vec::new(),
            MapProjection::Ortho => cascade_order(&self.cells),
            MapProjection::Persp => raise_order(&self.cells),
        };
        // A filter change captured the map as it looked; aim the tween from
        // there to the layout just built.
        if let Some(snapshot) = self.tween_capture.take() {
            let now_here: std::collections::HashSet<&Path> =
                self.cells.iter().map(|c| c.path.as_path()).collect();
            self.tween_from = snapshot
                .iter()
                .filter(|(cell, _, _)| now_here.contains(cell.path.as_path()))
                .map(|(cell, rect, depth)| {
                    (cell.path.clone(), TweenFrom { rect: *rect, depth: *depth })
                })
                .collect();
            self.tween_leavers = snapshot
                .into_iter()
                .filter(|(cell, _, _)| !now_here.contains(cell.path.as_path()))
                .collect();
            self.tween_start = Some(Instant::now());
        }
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

    /// The cell under a window point, if any. In the raised projections the
    /// test happens on the top faces (plus the riser it stands on), front-most
    /// first — the reverse of paint order, which is what "front" means.
    fn hit_cell(&self, pos: DVec2) -> Option<usize> {
        if self.projection == MapProjection::Flat || self.paint_order.len() != self.cells.len() {
            return treemap::hit(&self.cells, pos.x, pos.y);
        }
        let rise = self.rise();
        for &index in self.paint_order.iter().rev() {
            let cell = &self.cells[index];
            let mut top = self.project_rect(&cell.rect, self.elev(cell.depth));
            if self.projection == MapProjection::Ortho {
                top.h += rise;
            }
            if top.contains(pos.x, pos.y) {
                return Some(index);
            }
        }
        None
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
        let t = self.tween_t();
        let raised = self.projection != MapProjection::Flat;
        let rise = self.rise();

        cx.push_clip_rect(clip);
        self.draw_tile.begin_many_instances(cx);

        // Whatever the filter just dismissed fades out where it stood,
        // under everything that is staying.
        if let Some(t) = t {
            let ghost = (1.0 - t) as f32 * 0.9;
            for index in 0..self.tween_leavers.len() {
                let (rect, depth) = {
                    let (_, r, d) = &self.tween_leavers[index];
                    (*r, *d)
                };
                let (fill, _) = {
                    let (cell, _, _) = &self.tween_leavers[index];
                    self.tile_colors(cell, palette)
                };
                let z = if raised { self.elev_f(depth) } else { 0.0 };
                let top = self.project_rect(&rect, z);
                self.draw_tile.color = fade(fill, ghost);
                self.draw_tile.edge = fade(border_ink, ghost);
                self.draw_tile.cushion = 0.0;
                self.draw_tile.border = 0.5;
                self.draw_tile.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(top.x, top.y),
                        size: dvec2(top.w, top.h),
                    },
                );
            }
        }

        let order: Vec<usize> = if self.paint_order.len() == self.cells.len() {
            self.paint_order.clone()
        } else {
            (0..self.cells.len()).collect()
        };
        for &index in &order {
            let cell = &self.cells[index];
            let (vrect, vdepth, alpha) = self.tweened(cell, t);
            let alpha = alpha as f32;
            if alpha <= 0.02 {
                continue;
            }
            let z = if raised { self.elev_f(vdepth) } else { 0.0 };
            let top = self.project_rect(&vrect, z);
            let rect = Rect {
                pos: dvec2(top.x, top.y),
                size: dvec2(top.w, top.h),
            };
            let cell = &self.cells[index];
            let (fill, cushion) = self.tile_colors(cell, palette);
            let is_hover = Some(index) == hovered;
            let is_pick = picked.as_deref() == Some(cell.path.as_path()) && !cell.is_bundle();

            // The prism's body, under its own plate but over everything
            // already painted — which is exactly what one shared instance
            // batch in paint order gives.
            match self.projection {
                MapProjection::Flat => {}
                MapProjection::Ortho if z > 0.0 => {
                    // The riser: the face between this plate and the plateau
                    // it stands on. This is where "height means depth" is
                    // actually visible.
                    self.draw_tile.color = fade(scale_rgb(fill, 0.42), alpha);
                    self.draw_tile.edge = fade(border_ink, alpha);
                    self.draw_tile.cushion = 0.0;
                    self.draw_tile.border = 0.0;
                    self.draw_tile.draw_abs(
                        cx,
                        Rect {
                            pos: dvec2(top.x, top.y + top.h),
                            size: dvec2(top.w, rise),
                        },
                    );
                }
                MapProjection::Ortho => {}
                MapProjection::Persp => {
                    // A soft drop shadow sells the altitude the parallax
                    // implies; it grows with elevation.
                    let lift = 1.5 + z * 0.05;
                    self.draw_tile.color = Vec4f {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                        w: 0.32 * alpha,
                    };
                    self.draw_tile.edge = Vec4f::default();
                    self.draw_tile.cushion = 0.0;
                    self.draw_tile.border = 0.0;
                    self.draw_tile.draw_abs(
                        cx,
                        Rect {
                            pos: dvec2(top.x + lift * 0.6, top.y + lift),
                            size: dvec2(top.w, top.h),
                        },
                    );
                }
            }

            // Hover is the outline only — a bright border flash, never a
            // relit tile: on a dense map a whole rectangle changing value
            // under the pointer reads as the data changing.
            // The border is what separates siblings, and it can never be
            // allowed to eat the tile it surrounds — a three-point rectangle
            // with a one-point border on every side is all border. So it
            // scales with the tile and simply stops existing on the small
            // ones, where the cushion's own shading does the separating.
            let short = top.short_side();
            let border = if is_pick || is_hover {
                1.5
            } else {
                (short * 0.14).min(1.0)
            };
            let cushion = cushion * (CUSHION_FULL_AT / short.max(4.0)).clamp(0.30, 1.0) as f32;
            self.draw_tile.color = fade(fill, alpha);
            self.draw_tile.edge = fade(
                if is_pick {
                    accent
                } else if is_hover {
                    bright
                } else {
                    border_ink
                },
                alpha,
            );
            self.draw_tile.cushion = cushion;
            self.draw_tile.border = border as f32;
            self.draw_tile.draw_abs(cx, rect);

            if labels.len() >= LABEL_BUDGET {
                continue;
            }
            // A zoomed camera slides tiles half off the panel; a name pinned
            // to a corner nobody can see is a tile nobody can identify, so
            // labels clamp to the visible part of their rectangle — unless
            // almost none of it is visible, where a clamped name would just
            // pile up on the panel edge with its neighbours'.
            let at_x = (rect.pos.x + 4.0).max(clip.pos.x + 4.0);
            let room = rect.pos.x + rect.size.x - at_x - 4.0;
            let clamped = rect.pos.y < clip.pos.y;
            if clamped && rect.pos.y + rect.size.y - clip.pos.y < 40.0 {
                continue;
            }
            if cell.is_group
                && cell.header > 0.0
                && !(self.projection == MapProjection::Persp && cell.depth >= 2)
            {
                // Deep plates in perspective swell over their neighbours'
                // label strips; those names go quiet and live on the tooltip.
                // A group's name goes in the strip it reserved for it, which
                // is the only place on a group that its children are not
                // about to be drawn over — clamped on top of them when the
                // strip itself has slid off. In the raised projections the
                // children float up over that strip, so the name moves to
                // the plate's *bottom* edge, which the lift exposes instead.
                let at_y = if raised {
                    (rect.pos.y + rect.size.y - 13.0).min(clip.pos.y + clip.size.y - 13.0)
                } else {
                    (rect.pos.y + 1.0).max(clip.pos.y + 1.0)
                };
                labels.push(Label {
                    at: dvec2(at_x, at_y),
                    room,
                    line: format!("{}  {}", cell.name, treemap::format_bytes(cell.size)),
                    below: None,
                    ink: fade(bright, alpha),
                });
            } else if !cell.is_group
                && rect.size.x >= LABEL_MIN.x
                && rect.size.y >= LABEL_MIN.y
            {
                let two_lines = rect.size.y >= LABEL_TWO_LINE_H;
                labels.push(Label {
                    at: dvec2(at_x, (rect.pos.y + 3.0).max(clip.pos.y + 3.0)),
                    room,
                    line: cell.name.clone(),
                    below: two_lines.then(|| treemap::format_bytes(cell.size)),
                    ink: fade(ink_dark, alpha),
                });
            }
        }
        self.draw_tile.end_many_instances(cx);

        // The picked rectangle gets a ring on top of everything, because the
        // thing you are about to delete must be findable even when it is a
        // folder whose children cover it.
        if let Some(path) = &picked {
            let found = self
                .cells
                .iter()
                .position(|c| &c.path == path && !c.is_bundle());
            if let Some(index) = found {
                let cell = &self.cells[index];
                let (vrect, vdepth, _) = self.tweened(cell, t);
                let z = if raised { self.elev_f(vdepth) } else { 0.0 };
                let top = self.project_rect(&vrect, z);
                self.draw_tile.color = Vec4f { x: 0.0, y: 0.0, z: 0.0, w: 0.0 };
                self.draw_tile.edge = accent;
                self.draw_tile.cushion = 0.0;
                self.draw_tile.border = 2.0;
                self.draw_tile.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(top.x, top.y),
                        size: dvec2(top.w, top.h),
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
        // The active filter is never invisible: while one is on, the strip
        // says what it matched and offers the way out.
        self.filter_hit = Rect::default();
        if let Some((bytes, _)) = self.filter_matched() {
            let chip = format!(
                "matching {} of {} · clear",
                treemap::format_bytes(bytes),
                treemap::format_bytes(self.focused().size),
            );
            let width = text_width(&self.draw_bold, cx, &chip);
            right -= width;
            self.draw_bold.color = accent;
            self.draw_bold.draw_abs(cx, dvec2(right, strip.pos.y + 4.0), &chip);
            self.filter_hit = Rect {
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
                "Click picks · scroll zooms · drag pans · double-click fills the view · Esc backs \
                 out · right-click for the file menu"
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
        let top = self.project_rect(
            &cell.rect,
            match self.projection {
                MapProjection::Flat => 0.0,
                _ => self.elev(cell.depth),
            },
        );
        let anchor = dvec2(top.x, top.y);
        let cell_h = top.h;

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
        if self.filter_hit.contains(at) {
            if primary {
                self.set_filter(cx, None);
                cx.widget_action(self.uid, TreemapAction::FilterCleared);
            }
            return;
        }
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
        let rect = Rect {
            pos: dvec2(cell.rect.x, cell.rect.y),
            size: dvec2(cell.rect.w, cell.rect.h),
        };
        if cell.is_bundle() {
            // Nothing on disk is under there to act on — but zooming in on
            // it is exactly the right move: at the higher magnification the
            // re-layout dissolves the bundle into the things it stood for.
            if primary && taps >= 2 {
                self.fit_rect(cx, rect);
            }
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
            // The camera, not a re-root and not a navigation: the breadcrumb,
            // the browser and the scan all stay where they are — the map just
            // goes and looks at this one, and Esc backs straight out again.
            // (Going *to* a file lives in the context menu, on purpose.)
            self.fit_rect(cx, rect);
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

        // Layout before the chrome: the footer reads the pick's numbers and
        // the relayout is what refreshes them, so a frame that did both in
        // the other order would print a stale size and never come back for
        // the right one.
        if !self.tree.children.is_empty() {
            if self.laid_out != body || (self.stale && self.layout_is_due()) {
                self.relayout(body);
            } else if self.stale {
                // Drawn from a picture the scan has already moved past. The
                // throttle says not yet, so come back for it — a skipped
                // relayout that nothing ever comes back for is a map that
                // stops updating and never says so.
                self.frame = cx.new_next_frame();
            }
        }

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

        let labels = self.draw_map(cx, palette, body);
        self.draw_labels(cx, labels, body);
        self.draw_tooltip(cx, body, palette);

        // A running tween owns the frame clock; the frame after it ends
        // draws the exact target state, and only then is it let go of.
        if self.tween_t().is_some() {
            self.frame = cx.new_next_frame();
        } else if self.tween_start.is_some() {
            self.tween_start = None;
            self.tween_from.clear();
            self.tween_leavers.clear();
        }

        cx.add_aligned_rect_area(&mut self.area, rect);
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        if self.frame.is_event(event).is_some() {
            if self.tween_t().is_some() {
                self.redraw(cx);
            }
            if self.stale {
                if self.layout_is_due() {
                    self.redraw(cx);
                } else {
                    self.frame = cx.new_next_frame();
                }
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
                if primary {
                    // Click or pan — decided on release.
                    self.drag = Some(Drag {
                        from: e.abs,
                        cam_off: self.cam_off,
                        taps: e.tap_count,
                    });
                    self.panning = false;
                } else {
                    // A secondary press acts at once: the context menu it is
                    // about to open needs its target picked now.
                    self.press(cx, e.abs, e.tap_count, false);
                }
            }
            Hit::FingerMove(e) => {
                if let Some(drag) = self.drag {
                    let delta = e.abs - drag.from;
                    if !self.panning && delta.length() > 4.0 {
                        self.panning = true;
                    }
                    if self.panning && self.cam_scale > 1.001 {
                        // The map follows the finger — dragging is how a
                        // zoomed view gets around.
                        self.set_camera(cx, self.cam_scale, drag.cam_off - delta);
                    }
                }
            }
            Hit::FingerUp(_) => {
                if let Some(drag) = self.drag.take() {
                    if !self.panning {
                        self.press(cx, drag.from, drag.taps, true);
                    }
                }
                self.panning = false;
            }
            Hit::FingerScroll(e) => {
                // Wheel/two fingers zoom about the pointer. The exponent
                // makes equal wheel travel worth equal zoom *ratio*, which
                // is the only way in and out feel like the same control.
                let factor = (-e.scroll.y * 0.011).exp();
                self.zoom_at(cx, e.abs, factor);
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

    /// Re-open the current root under the current scan rules, cache welcome.
    pub fn remap(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.remap(cx);
        }
    }

    /// Choose how the map projects: flat, extruded, or perspective.
    pub fn set_projection(&self, cx: &mut Cx, projection: MapProjection) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_projection(cx, projection);
        }
    }

    /// Apply (or clear, with None) the live filter.
    pub fn set_filter(&self, cx: &mut Cx, filter: Option<Query>) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.set_filter(cx, filter);
        }
    }

    /// Bytes per kind tag under the mapped folder, for the legend.
    pub fn kind_totals(&self, _cx: &mut Cx) -> [u64; 16] {
        self.borrow_mut().map(|mut i| i.kind_totals()).unwrap_or([0; 16])
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

fn lerp_rect(a: &MapRect, b: &MapRect, t: f64) -> MapRect {
    MapRect {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
        w: a.w + (b.w - a.w) * t,
        h: a.h + (b.h - a.h) * t,
    }
}

/// The paint order for the vertically-extruded map: everything only ever
/// leans *north* (up the screen), so a cell may only cover cells above it —
/// paint north before south. Nesting still means "parent under child", so
/// the sort happens among siblings and each subtree stays together. The
/// input is the layout's pre-order, which keeps every subtree contiguous.
fn cascade_order(cells: &[Cell]) -> Vec<usize> {
    fn emit(cells: &[Cell], start: usize, end: usize, depth: usize, out: &mut Vec<usize>) {
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        let mut i = start;
        while i < end {
            let s = i;
            i += 1;
            while i < end && cells[i].depth > depth {
                i += 1;
            }
            blocks.push((s, i));
        }
        blocks.sort_by(|a, b| {
            cells[a.0]
                .rect
                .y
                .partial_cmp(&cells[b.0].rect.y)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (s, e) in blocks {
            out.push(s);
            emit(cells, s + 1, e, depth + 1, out);
        }
    }
    let mut out = Vec::with_capacity(cells.len());
    emit(cells, 0, cells.len(), 0, &mut out);
    out
}

/// The paint order for the perspective map: nothing at a lower elevation can
/// ever be in front of something higher under a straight-down eye, so floor
/// to sky is correct. Stable, so the layout's order settles equal depths.
fn raise_order(cells: &[Cell]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..cells.len()).collect();
    order.sort_by_key(|&i| cells[i].depth);
    order
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

    fn cell(name: &str, depth: usize, y: f64) -> Cell {
        Cell {
            path: PathBuf::from(format!("/{name}")),
            name: name.to_string(),
            size: 1,
            files: 1,
            is_dir: true,
            kind: 0,
            depth,
            rect: MapRect { x: 0.0, y, w: 10.0, h: 10.0 },
            is_group: true,
            header: 0.0,
            pending: false,
            extra: 0,
        }
    }

    // The rule that makes the extruded map paint correctly: everything only
    // ever leans north, so north paints first — among siblings, with each
    // subtree kept together and parents under their children.
    #[test]
    fn the_cascade_paints_north_before_south_and_parents_before_children() {
        // Pre-order: P(y=50) with children c1(y=90), c2(y=60); then Q(y=0).
        let cells = vec![
            cell("p", 0, 50.0),
            cell("c1", 1, 90.0),
            cell("c2", 1, 60.0),
            cell("q", 0, 0.0),
        ];
        let order = cascade_order(&cells);
        let names: Vec<&str> = order.iter().map(|&i| cells[i].name.as_str()).collect();
        assert_eq!(names, vec!["q", "p", "c2", "c1"]);
    }

    #[test]
    fn the_perspective_paints_floor_to_sky() {
        let cells = vec![cell("deep", 3, 0.0), cell("shallow", 0, 50.0), cell("mid", 1, 9.0)];
        let order = raise_order(&cells);
        let depths: Vec<usize> = order.iter().map(|&i| cells[i].depth).collect();
        assert_eq!(depths, vec![0, 1, 3]);
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
