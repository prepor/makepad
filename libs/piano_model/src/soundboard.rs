// Shared modal soundboard: turns bridge force into radiated sound at TWO
// listening points, with the bridge treated as four coupling regions (the
// bass bridge and three sections of the long bridge) rather than one
// scalar point.
//
// Space, in three physical layers:
// - COUPLING: each board mode is driven through a per-(mode, region)
//   weight. Long-wavelength modes span the whole board, so every bridge
//   region drives them coherently; short-wavelength modes alternate sign
//   region-to-region the way real mode shapes do. This is what lets a bass
//   note and a treble note excite the same board differently — the board
//   itself has a left-right image, not just the panned direct strings.
// - RADIATION / IMAGE: the two listening points hear each mode with a
//   per-mode interchannel phase phi = 2 pi f tau, where tau is a
//   physically sized time difference (sub-millisecond) set by the region's
//   azimuth plus a small per-mode scatter, and a mild level difference
//   that grows with frequency (radiation lobes). Low modes therefore stay
//   nearly coherent between channels and the coherence falls raggedly
//   with frequency — the measured interaural envelope of a real
//   instrument in a room. An earlier scheme read Im(z) left / Re(z) right
//   with independent random signs: a blanket 90-degree offset that pinned
//   midrange interchannel correlation near zero and read as a phasey
//   wash rather than an instrument with a place.
// - The per-region DIRECT (velocity-coupled) component is panned by its
//   region azimuth, so the instant part of the sound sits in the same
//   image as the modal part.
//
// Mode damping is pinned to measured soundboard physics (see params.rs:
// board_sig_*): quality factors ~19-37 across the band, the Giordano
// range. The attack still speaks instantly through the direct paths; the
// board's own modes bloom over tens of milliseconds underneath, which is
// the "body" of a large instrument.

use crate::modal::{pad8, run_modes_stereo, KernelPath, MAX_CHUNK};
use crate::params::DesignParams;

pub const BOARD_REGIONS: usize = 4;
/// Hard cap on the parameterised per-region mode count (preallocation bound).
pub const BOARD_MODES_MAX: usize = 256;

/// Region azimuths in pan units (player perspective, bass left) and the
/// interchannel time difference (seconds) each azimuth produces at a
/// close listening position.
const REGION_AZ: [f64; BOARD_REGIONS] = [-0.42, -0.14, 0.12, 0.38];
const AZ_ITD_S: f64 = 0.00045;

/// Radiativity of the instrument body: how strongly a bridge-force partial
/// at `f` Hz reaches the listener, relative to the mid plateau.
///
/// Shape (normalised, dimensionless, ~1.0 across the plateau):
/// - falls below ~100 Hz (the board is smaller than the wavelength)
/// - broad plateau ~150 Hz .. ~2 kHz with a low-mid body emphasis
/// - gentle roll-off above the top corner
pub fn radiativity(f: f64, p: &DesignParams) -> f64 {
    let hp1 = f / (f + p.rad_hp1);
    let hp2 = f / (f + p.rad_hp2);
    let x = (f / p.rad_lp) * (f / p.rad_lp);
    let lp = (1.0 / (1.0 + x)).powf(0.5 * p.rad_lp_pow);
    // Low-mid body emphasis: the board's main resonances sit in the
    // ~100-400 Hz region and radiate the fundamentals of the middle octaves
    // strongly — the reference recordings have the FUNDAMENTAL as the
    // strongest partial through C3..C4 even though the strike comb feeds
    // partial 2 five dB more force. A plateau that is flat down to the
    // bass high-pass cannot reproduce that and reads as thin ("tinny").
    // Fourth-order shelf keeps the emphasis out of the 500 Hz+ region, and
    // a second-order high-pass at 85 Hz keeps it out of the deep bass:
    // below the main resonance the board stops radiating again, and a
    // C2 whose fundamental outweighs its partial cluster reads as boom,
    // not body (the bass-speaks-through-partials law).
    let b = f / p.rad_body_hz;
    let b4 = b * b * b * b;
    let body = 1.0 + p.rad_body * (1.0 / (1.0 + b4)) * (f * f / (f * f + 85.0 * 85.0));
    hp1 * hp2 * lp * body
}

pub struct Soundboard {
    // Region banks concatenated: region r occupies [r*n .. (r+1)*n).
    zr: Vec<f32>,
    zi: Vec<f32>,
    cr: Vec<f32>,
    ci: Vec<f32>,
    gin: Vec<f32>,
    gout_l: Vec<f32>,
    gout_ri: Vec<f32>,
    gout_rr: Vec<f32>,
    n_padded: usize,
    /// Direct radiation plateau gain (velocity coupling), per region.
    pub direct: f32,
    /// Diagnostic scale on the direct path (1.0 in normal use).
    pub dbg_direct: f32,
    dx1: [f32; BOARD_REGIONS],
    dlp: [f32; BOARD_REGIONS],
    dlp2: [f32; BOARD_REGIONS],
    dir_pl: [f32; BOARD_REGIONS],
    dir_pr: [f32; BOARD_REGIONS],
    dc_lp: f32,
    dc_lp2: f32,
}

fn hash01(mut x: u32) -> f32 {
    // deterministic per-mode jitter
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    (x >> 8) as f32 * (1.0 / 16_777_216.0)
}

impl Soundboard {
    pub fn new(sample_rate: f64, p: &DesignParams) -> Self {
        let modes = (p.board_modes as usize).clamp(16, BOARD_MODES_MAX);
        let n = pad8(modes);
        let total = n * BOARD_REGIONS;
        let mut cr = vec![0.0f32; total];
        let mut ci = vec![0.0f32; total];
        let mut gin = vec![0.0f32; total];
        let mut gout_l = vec![0.0f32; total];
        let mut gout_ri = vec![0.0f32; total];
        let mut gout_rr = vec![0.0f32; total];
        let dt = 1.0 / sample_rate;
        let norm = 1.0 / (modes as f64).sqrt();
        let mut dir_pl = [0.0f32; BOARD_REGIONS];
        let mut dir_pr = [0.0f32; BOARD_REGIONS];
        for (r, az) in REGION_AZ.iter().enumerate() {
            let ang = ((az * 0.5 + 1.0) * core::f64::consts::FRAC_PI_4) as f32;
            dir_pl[r] = core::f32::consts::SQRT_2 * ang.cos();
            dir_pr[r] = core::f32::consts::SQRT_2 * ang.sin();
        }
        for m in 0..modes {
            let jitter = 0.94 + 0.12 * hash01(m as u32 * 3 + 1) as f64;
            // Lattice from 55 Hz: a concert grand's first board resonance
            // sits ~55-90 Hz. Starting at 60 left C2's fundamental hanging
            // on the sparse jittered edge of the stack (a 13 dB notch);
            // starting at 47 handed the BOTTOM octave's fundamentals a
            // dedicated resonance no real board gives them (F#1 measured
            // +17 dB over the recorded-piano trend: boom, not body).
            let f = 55.0 * p.board_ratio.powi(m as i32) * jitter;
            if f >= 0.45 * sample_rate {
                continue;
            }
            // The first resonances are strongly radiation-loaded (that is
            // what makes a board a radiator): extra damping below ~150 Hz
            // widens the bottom modes so the lattice edge is smooth. With
            // laboratory low-mid Q at the bottom too, the 3-4 Hz mode
            // spacing left audible notches between modes — C2's
            // fundamental measured 13 dB into one such gap.
            let sigma_rad = 18.0 / (1.0 + (f / 85.0) * (f / 85.0));
            let sigma = (p.board_sig_base + sigma_rad + f / p.board_sig_div).min(400.0);
            let rr = (-sigma * dt).exp();
            let th = core::f64::consts::TAU * f * dt;
            let (crm, cim) = ((rr * th.cos()) as f32, (rr * th.sin()) as f32);
            // Radiativity curve: mid plateau + body emphasis, dark top.
            let tilt = p.board_tilt * radiativity(f, p);
            // radiation lobe sign, common to both listening points
            let s = if hash01(m as u32 * 11 + 4) < 0.5 { -1.0 } else { 1.0 };
            // base radiated amplitude (state-integration normalised)
            let fs_norm = 48000.0 / sample_rate;
            let g0 = s * tilt * norm * sigma * 0.006 * fs_norm
                * (0.75 + 0.5 * hash01(m as u32 * 5 + 2) as f64);
            // How coherently the bridge regions drive this mode: a mode
            // whose half-wavelength exceeds the bridge span is pushed the
            // same way by every region; short modes alternate.
            let coh = 1.0 / (1.0 + (f / 180.0) * (f / 180.0));
            for r in 0..BOARD_REGIONS {
                let i = r * n + m;
                cr[i] = crm;
                ci[i] = cim;
                let sgn = if hash01((m as u32) * 17 + r as u32 * 7 + 9) < 0.5 { -1.0 } else { 1.0 };
                gin[i] = (coh / BOARD_REGIONS as f64
                    + (1.0 - coh) * sgn / (BOARD_REGIONS as f64).sqrt()) as f32;
                // interchannel phase: region azimuth ITD + per-mode scatter
                let tau = REGION_AZ[r] * AZ_ITD_S
                    + (hash01(m as u32 * 23 + r as u32 * 13 + 3) as f64 - 0.5) * 0.00044;
                let phi = core::f64::consts::TAU * f * tau;
                // level image: region pan, plus a lobe-level difference
                // that grows with frequency
                let lobe = 1.0 + (0.15 + 0.95 * (f / 4000.0).min(1.0))
                    * (hash01(m as u32 * 29 + r as u32 * 19 + 5) as f64 - 0.5);
                let al = g0 * dir_pl[r] as f64;
                let ar = g0 * dir_pr[r] as f64 * lobe;
                gout_l[i] = al as f32;
                gout_ri[i] = (ar * phi.cos()) as f32;
                gout_rr[i] = (ar * phi.sin()) as f32;
            }
        }
        Self {
            zr: vec![0.0f32; total],
            zi: vec![0.0f32; total],
            cr,
            ci,
            gin,
            gout_l,
            gout_ri,
            gout_rr,
            n_padded: n,
            // Plateau-normalised velocity coupling (see RadTilt in lib.rs).
            direct: (p.board_direct * sample_rate / (core::f64::consts::TAU * p.rad_vel_hz)) as f32,
            dbg_direct: 1.0,
            dx1: [0.0; BOARD_REGIONS],
            dlp: [0.0; BOARD_REGIONS],
            dlp2: [0.0; BOARD_REGIONS],
            dir_pl,
            dir_pr,
            dc_lp: (1.0 - (-core::f64::consts::TAU * p.rad_vel_hz / sample_rate).exp()) as f32,
            dc_lp2: (1.0 - (-core::f64::consts::TAU * p.rad_lp / sample_rate).exp()) as f32,
        }
    }

    /// Accumulates board response to the per-region bridge-force inputs.
    pub fn render(
        &mut self,
        path: KernelPath,
        inputs: &[[f32; MAX_CHUNK]; BOARD_REGIONS],
        n: usize,
        l: &mut [f32],
        r: &mut [f32],
    ) {
        debug_assert!(n <= MAX_CHUNK);
        let np = self.n_padded;
        for reg in 0..BOARD_REGIONS {
            let a = reg * np;
            let b = a + np;
            run_modes_stereo(
                path,
                &mut self.zr[a..b],
                &mut self.zi[a..b],
                &self.cr[a..b],
                &self.ci[a..b],
                &self.gin[a..b],
                &self.gout_l[a..b],
                &self.gout_ri[a..b],
                &self.gout_rr[a..b],
                &inputs[reg][..n],
                1.0,
                l,
                r,
            );
            // per-region velocity-coupled direct radiation, panned by the
            // region's azimuth: differentiate, flatten at the bottom of the
            // plateau, roll off above the top corner
            for k in 0..n {
                let x = inputs[reg][k];
                let diff = x - self.dx1[reg];
                self.dx1[reg] = x;
                self.dlp[reg] += self.dc_lp * (diff - self.dlp[reg]);
                self.dlp2[reg] += self.dc_lp2 * (self.dlp[reg] - self.dlp2[reg]);
                let d = self.dbg_direct * self.direct * self.dlp2[reg];
                l[k] += self.dir_pl[reg] * d;
                r[k] += self.dir_pr[reg] * d;
            }
        }
    }

    pub fn reset(&mut self) {
        self.zr.fill(0.0);
        self.zi.fill(0.0);
        self.dx1 = [0.0; BOARD_REGIONS];
        self.dlp = [0.0; BOARD_REGIONS];
        self.dlp2 = [0.0; BOARD_REGIONS];
    }
}
