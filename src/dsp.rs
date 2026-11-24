//! Built-in vinyl emulation. Real-time safe: all memory is allocated in
//! `Vinyl::new`, `tick` does no allocation, locking or syscalls.
//!
//! Signal chain:
//!
//! ```text
//! modulated delay (wow at the rotation rate + its 2nd harmonic, flutter, drift)
//!   -> HP 28 Hz -> low-shelf warmth
//!   -> RIAA-style treble pre-emphasis -> offset tanh (stylus) -> de-emphasis -> DC block
//!   -> 2x LP (wear-dependent cutoff)
//!   -> M/S: side high-passed at 150 Hz (mono bass, as cut on a lathe), scaled by width
//!   + crackle: dust ticks and pops, scratches that repeat every revolution
//!   + groove noise (band-limited, side-heavy, swelling once per revolution)
//!   -> dry/wet (dry is read from the same delay line, so no comb filtering)
//! ```
//!
//! Tuned for small Bluetooth speakers such as the Marshall Emberton III:
//! nothing is generated below ~150 Hz (rumble and warp infrasound are inaudible there
//! but eat headroom and pump the speaker's limiter), and noise stays below ~7 kHz,
//! where SBC/AAC encode noise-like content cleanly.

use crate::params::Params;
use std::f32::consts::{PI, TAU};

const DELAY_LEN: usize = 8192; // power of two; ~43 ms @ 192 kHz
const DELAY_MASK: usize = DELAY_LEN - 1;
const BASE_DELAY_MS: f32 = 3.0;
const DENORM: f32 = 1.0e-18;

/// Platter rotation rate for each speed in `params::RPM_LABELS`.
pub const ROTATION_HZ: [f32; 3] = [100.0 / 3.0 / 60.0, 45.0 / 60.0, 78.26 / 60.0];

/// The groove carries treble boosted around the RIAA 75 µs corner, so stylus distortion
/// lands mostly on cymbals and sibilants.
const PRE_EMPH_HZ: f32 = 2_122.0;
const PRE_EMPH_DB: f32 = 8.0;
/// Below this a cutting lathe sums the channels to mono.
const MONO_BASS_HZ: f32 = 150.0;
/// Measured RMS of the raw groove-noise generator (see `noise_generator_rms` test).
const NOISE_GEN_RMS: f32 = 0.273;

const VOICES: usize = 16;
const SCRATCHES: usize = 4;

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }
}

impl Biquad {
    fn set(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        let inv = 1.0 / a0;
        self.b0 = b0 * inv;
        self.b1 = b1 * inv;
        self.b2 = b2 * inv;
        self.a1 = a1 * inv;
        self.a2 = a2 * inv;
    }

    fn w0(fs: f32, f: f32) -> (f32, f32) {
        let w = 2.0 * PI * f.clamp(5.0, fs * 0.45) / fs;
        (w.cos(), w.sin())
    }

    pub fn lowpass(&mut self, fs: f32, f: f32, q: f32) {
        let (c, s) = Self::w0(fs, f);
        let alpha = s / (2.0 * q);
        self.set(
            (1.0 - c) / 2.0,
            1.0 - c,
            (1.0 - c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        );
    }

    pub fn highpass(&mut self, fs: f32, f: f32, q: f32) {
        let (c, s) = Self::w0(fs, f);
        let alpha = s / (2.0 * q);
        self.set(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        );
    }

    pub fn lowshelf(&mut self, fs: f32, f: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let (c, s) = Self::w0(fs, f);
        let alpha = s / 2.0 * 2f32.sqrt(); // shelf slope S = 1
        let sa = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) - (a - 1.0) * c + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sa),
            (a + 1.0) + (a - 1.0) * c + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sa,
        );
    }

    /// RBJ high shelf; shelves of +g and -g dB at the same corner cancel exactly.
    pub fn highshelf(&mut self, fs: f32, f: f32, gain_db: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let (c, s) = Self::w0(fs, f);
        let alpha = s / 2.0 * 2f32.sqrt();
        let sa = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        );
    }

    #[inline(always)]
    pub fn tick(&mut self, x: f32) -> f32 {
        // Transposed direct form II
        let x = x + DENORM;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

#[derive(Clone, Copy, Default)]
struct OnePole {
    a: f32,
    z: f32,
}

impl OnePole {
    fn set_lp(&mut self, fs: f32, f: f32) {
        self.a = 1.0 - (-2.0 * PI * f / fs).exp();
    }

    #[inline(always)]
    fn lp(&mut self, x: f32) -> f32 {
        self.z += self.a * (x - self.z) + DENORM;
        self.z
    }

    #[inline(always)]
    fn hp(&mut self, x: f32) -> f32 {
        x - self.lp(x)
    }
}

/// One ringing click: complex phasor rotating by (cr, ci) = decay * e^{iω}.
#[derive(Clone, Copy, Default)]
struct Voice {
    re: f32,
    im: f32,
    cr: f32,
    ci: f32,
    gl: f32,
    gr: f32,
}

/// A scratch across the grooves: the stylus hits it once per revolution,
/// always at the same angle and with the same channel balance, until the
/// groove spirals past it.
#[derive(Clone, Copy, Default)]
struct Scratch {
    angle: f32, // 0..1 of a revolution
    amp: f32,   // relative to click level
    freq: f32,
    gl: f32,
    gr: f32,
    revs_left: u32,
    idle_revs: u32,
}

/// Groove noise colour: mostly low-mid "roar" with some hiss, nothing below 150 Hz.
#[derive(Clone, Copy, Default)]
struct NoiseShaper {
    body: OnePole,
    hiss: OnePole,
    low_cut: [OnePole; 2],
}

impl NoiseShaper {
    fn set(&mut self, fs: f32) {
        self.body.set_lp(fs, 1_500.0);
        self.hiss.set_lp(fs, 6_500.0);
        for f in &mut self.low_cut {
            f.set_lp(fs, 150.0);
        }
    }

    #[inline(always)]
    fn shape(&mut self, white: f32) -> f32 {
        let y = self.body.lp(white) + 0.35 * self.hiss.lp(white);
        let y = self.low_cut[0].hp(y);
        self.low_cut[1].hp(y)
    }
}

#[derive(Clone, Copy, Default)]
struct Chan {
    hp: Biquad,
    shelf: Biquad,
    pre: Biquad,
    de: Biquad,
    dc: OnePole,
    lp1: Biquad,
    lp2: Biquad,
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Snapshot {
    wet: f32,
    age: f32,
    crackle: f32,
    wow: f32,
    noise: f32,
    drive: f32,
    width: f32,
    rpm: usize,
}

pub struct Vinyl {
    fs: f32,
    version: u32,
    p: Snapshot,
    ch: [Chan; 2],
    side_hp: Biquad,
    dl: [Box<[f32]>; 2],
    w: usize,
    base_delay: f32,
    // rotation and pitch modulation
    rot_ph: f32, // 0..1
    rot_inc: f32,
    rot_sin: f32,
    flut_ph: f32,
    flut_inc: f32,
    wow_amp: f32,  // samples
    flut_amp: f32, // samples
    drift: f32,
    drift_target: f32,
    ctr: u32,
    // stylus distortion
    sat_on: bool,
    sat_k: f32,
    sat_inv_k: f32,
    sat_c2: f32,
    // crackle
    rng: u32,
    voices: [Voice; VOICES],
    next_voice: usize,
    tick_p: f32,
    pop_p: f32,
    fizz_p: f32,
    click_lvl: f32,
    density: f32,
    density_target: f32,
    scratches: [Scratch; SCRATCHES],
    n_scratches: usize,
    // groove noise
    noise_mid: NoiseShaper,
    noise_side: NoiseShaper,
    noise_lvl: f32,
}

impl Vinyl {
    pub fn new(sample_rate: f64) -> Self {
        let fs = sample_rate as f32;
        let mut v = Self {
            fs,
            version: 0,
            p: Snapshot::default(),
            ch: [Chan::default(); 2],
            side_hp: Biquad::default(),
            dl: [
                vec![0.0; DELAY_LEN].into_boxed_slice(),
                vec![0.0; DELAY_LEN].into_boxed_slice(),
            ],
            w: 0,
            base_delay: BASE_DELAY_MS * fs / 1000.0,
            rot_ph: 0.0,
            rot_inc: ROTATION_HZ[0] / fs,
            rot_sin: 0.0,
            flut_ph: 0.0,
            flut_inc: 2.0 * PI * 6.5 / fs,
            wow_amp: 0.0,
            flut_amp: 0.0,
            drift: 0.0,
            drift_target: 0.0,
            ctr: 0,
            sat_on: false,
            sat_k: 1.0,
            sat_inv_k: 1.0,
            sat_c2: 0.0,
            rng: 0x9E37_79B9,
            voices: [Voice::default(); VOICES],
            next_voice: 0,
            tick_p: 0.0,
            pop_p: 0.0,
            fizz_p: 0.0,
            click_lvl: 0.0,
            density: 1.0,
            density_target: 1.0,
            scratches: [Scratch::default(); SCRATCHES],
            n_scratches: 0,
            noise_mid: NoiseShaper::default(),
            noise_side: NoiseShaper::default(),
            noise_lvl: 0.0,
        };
        v.noise_mid.set(fs);
        v.noise_side.set(fs);
        v.recompute();
        v
    }

    /// Pull parameters if they changed. Call once per audio block.
    #[inline]
    pub fn update(&mut self, params: &Params) {
        let ver = params.version();
        if ver == self.version {
            return;
        }

        self.version = ver;
        self.p = Snapshot {
            wet: params.wet.get(),
            age: params.age.get(),
            crackle: params.crackle.get(),
            wow: params.wow.get(),
            noise: params.noise.get(),
            drive: params.drive.get(),
            width: params.width.get(),
            rpm: params.rpm_index().min(ROTATION_HZ.len() - 1),
        };
        self.recompute();
    }

    fn recompute(&mut self) {
        let fs = self.fs;
        let p = self.p;

        let cutoff = 18_000.0 * (5_000.0f32 / 18_000.0).powf(p.age.powf(1.2));
        for c in &mut self.ch {
            c.hp.highpass(fs, 28.0, 0.707);
            c.shelf.lowshelf(fs, 200.0, 2.5 * p.age);
            c.pre.highshelf(fs, PRE_EMPH_HZ, PRE_EMPH_DB);
            c.de.highshelf(fs, PRE_EMPH_HZ, -PRE_EMPH_DB);
            c.dc.set_lp(fs, 8.0);
            c.lp1.lowpass(fs, cutoff, 0.6);
            c.lp2.lowpass(fs, cutoff * 1.15, 0.75);
        }
        self.side_hp.highpass(fs, MONO_BASS_HZ, 0.707);

        // Wow comes from an off-centre hole, so it runs at the rotation rate.
        // Peak pitch deviation: wow up to 0.45 %, flutter up to 0.08 %.
        let rot_hz = ROTATION_HZ[p.rpm];
        self.rot_inc = rot_hz / fs;
        self.wow_amp = p.wow * 0.0045 / (TAU * rot_hz) * fs;
        self.flut_amp = p.wow * 0.0008 / (TAU * 6.5) * fs;

        // Stylus: an even-order term sets the 2nd harmonic (about -40 dB at 1 kHz, -6 dBFS
        // for "warm"); a gentle tanh keeps the 3rd below it. Worn grooves track worse.
        self.sat_on = p.drive > 0.01;
        self.sat_k = 0.1 + 0.6 * p.drive;
        self.sat_inv_k = 1.0 / self.sat_k;
        self.sat_c2 = 0.14 * p.drive * (1.0 + p.age);

        // Crackle: ticks up to ~25/s, pops up to ~0.4/s, dense quiet fizz, 0..4 scratches.
        let c = p.crackle;
        self.tick_p = (c.powf(1.5) * 25.0) / fs;
        self.pop_p = (c * 0.4) / fs;
        self.fizz_p = (c * 300.0) / fs;
        self.click_lvl = 0.12 * c;
        self.n_scratches = ((c * SCRATCHES as f32).round() as usize).min(SCRATCHES);

        // Groove noise from about -72 dBFS RMS (new record) to -42 dBFS (worn out).
        self.noise_lvl = if p.noise > 0.0 {
            10f32.powf((-72.0 + 30.0 * p.noise) / 20.0) / NOISE_GEN_RMS
        } else {
            0.0
        };
    }

    #[inline(always)]
    fn rand(&mut self) -> f32 {
        // xorshift32 -> [0, 1)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    #[inline(always)]
    fn bipolar(&mut self) -> f32 {
        self.rand() * 2.0 - 1.0
    }

    #[inline(always)]
    fn read_frac(buf: &[f32], w: usize, d: f32) -> f32 {
        let pos = w as f32 - d;
        let i = pos.floor();
        let frac = pos - i;
        let i0 = (i as isize as usize) & DELAY_MASK;
        let i1 = (i0 + 1) & DELAY_MASK;
        buf[i0] + (buf[i1] - buf[i0]) * frac
    }

    /// Start a damped sinusoid: the needle hitting a particle rings the
    /// stylus/cartridge briefly at a random resonance.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn trigger(&mut self, amp: f32, f_lo: f32, f_hi: f32, tau_lo: f32, tau_hi: f32, gl: f32, gr: f32) {
        let f = f_lo + (f_hi - f_lo) * self.rand();
        let tau = tau_lo + (tau_hi - tau_lo) * self.rand();
        let w = 2.0 * PI * f / self.fs;
        let decay = (-1.0 / (tau * self.fs)).exp();
        let sign = if self.rand() < 0.5 { -1.0 } else { 1.0 };
        let v = &mut self.voices[self.next_voice];
        self.next_voice = (self.next_voice + 1) % VOICES;
        *v = Voice {
            re: amp * sign,
            im: 0.0,
            cr: w.cos() * decay,
            ci: w.sin() * decay,
            gl,
            gr,
        };
    }

    /// Random channel balance; the vertical part of groove motion shows up
    /// as anti-phase between the channels.
    #[inline(always)]
    fn random_gains(&mut self, spread: f32) -> (f32, f32) {
        let pan = self.bipolar() * spread;
        let polarity = if self.rand() < 0.35 { -1.0 } else { 1.0 };
        (1.0 - pan, (1.0 + pan) * polarity)
    }

    /// Once per revolution: scratches age, fade out and new ones appear.
    fn on_revolution(&mut self) {
        for i in 0..SCRATCHES {
            if i >= self.n_scratches {
                self.scratches[i].revs_left = 0;
                continue;
            }

            let s = self.scratches[i];
            if s.revs_left > 0 {
                self.scratches[i].revs_left -= 1;
                if s.revs_left == 1 {
                    self.scratches[i].idle_revs = 2 + (self.rand() * 10.0) as u32;
                }
            } else if s.idle_revs > 0 {
                self.scratches[i].idle_revs -= 1;
            } else {
                let (gl, gr) = self.random_gains(0.7);
                self.scratches[i] = Scratch {
                    angle: self.rand(),
                    amp: 0.5 + 0.7 * self.rand(),
                    freq: 700.0 + 1_800.0 * self.rand(),
                    gl,
                    gr,
                    revs_left: 6 + (self.rand() * 30.0) as u32,
                    idle_revs: 0,
                };
            }
        }
    }

    /// Returns (left, right) crackle contribution. `prev` is the rotation phase before this sample.
    #[inline(always)]
    fn crackle(&mut self, prev: f32) -> (f32, f32) {
        let cur = self.rot_ph;
        let lvl = self.click_lvl;

        // Scratches fire when the platter passes their angle (most of the time: the stylus
        // sometimes rides over).
        for i in 0..self.n_scratches {
            let s = self.scratches[i];
            let crossed = if cur >= prev {
                prev < s.angle && s.angle <= cur
            } else {
                s.angle > prev || s.angle <= cur
            };
            if s.revs_left > 0 && crossed && self.rand() < 0.93 {
                let a = lvl * s.amp * (0.75 + 0.5 * self.rand());
                self.trigger(a, s.freq * 0.85, s.freq * 1.15, 0.00015, 0.0004, s.gl, s.gr);
                self.trigger(a * 0.5, 160.0, 320.0, 0.001, 0.002, s.gl, s.gr);
            }
        }

        // Dust comes in patches: the density wanders every ~1.4 s.
        if self.ctr & 0xFFFF == 0 {
            self.density_target = 0.4 + 1.2 * self.rand();
        }
        self.density += (self.density_target - self.density) * 1.0e-5;
        let dn = self.density;

        let u = self.rand();
        if u < self.pop_p * dn {
            let a = lvl * (1.0 + self.rand());
            let (gl, gr) = self.random_gains(0.2);
            self.trigger(a, 250.0, 900.0, 0.0006, 0.0015, gl, gr);
        } else if u < (self.pop_p + self.tick_p) * dn {
            // log-distributed amplitude over ~30 dB: mostly faint, occasionally sharp
            let a = lvl * 10f32.powf(-1.5 * self.rand());
            let (gl, gr) = self.random_gains(0.6);
            self.trigger(a, 1_800.0, 6_000.0, 0.00005, 0.0002, gl, gr);
        } else if u < (self.pop_p + self.tick_p + self.fizz_p) * dn {
            let a = lvl * 0.04 * self.rand();
            let (gl, gr) = self.random_gains(1.0);
            self.trigger(a, 3_000.0, 7_000.0, 0.00002, 0.00006, gl, gr);
        }

        let (mut l, mut r) = (0.0, 0.0);
        for v in &mut self.voices {
            if v.re == 0.0 && v.im == 0.0 {
                continue;
            }

            let (re, im) = (v.re * v.cr - v.im * v.ci, v.re * v.ci + v.im * v.cr);
            if re.abs() + im.abs() < 1.0e-6 {
                v.re = 0.0;
                v.im = 0.0;
                continue;
            }
            v.re = re;
            v.im = im;
            l += im * v.gl;
            r += im * v.gr;
        }
        (l, r)
    }

    /// Groove noise, side-heavy (vertical stylus motion) and swelling once per revolution.
    #[inline(always)]
    fn groove_noise(&mut self) -> (f32, f32) {
        let (a, b) = (self.bipolar(), self.bipolar());
        let m = self.noise_mid.shape(a);
        let s = self.noise_side.shape(b);
        let g = self.noise_lvl * (1.0 + 0.3 * self.rot_sin);
        ((0.7 * m + s) * g, (0.7 * m - s) * g)
    }

    /// Stylus tracing distortion on one sample, in the pre-emphasised (groove) domain.
    #[inline(always)]
    fn stylus(&self, x: f32) -> f32 {
        let x2 = x * x;
        (self.sat_k * x).tanh() * self.sat_inv_k + self.sat_c2 * x2 / (1.0 + x2)
    }

    /// Process one stereo frame.
    #[inline(always)]
    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let w = self.w;
        self.dl[0][w] = l;
        self.dl[1][w] = r;

        // --- platter: rotation phase drives wow, scratches and noise swell
        let prev = self.rot_ph;
        self.rot_ph += self.rot_inc;
        if self.rot_ph >= 1.0 {
            self.rot_ph -= 1.0;
            self.on_revolution();
        }
        let rot = TAU * self.rot_ph;
        self.rot_sin = rot.sin();

        // --- pitch modulation (slow random drift retargeted every 1024 samples)
        self.ctr = self.ctr.wrapping_add(1);
        if self.ctr & 1023 == 0 {
            self.drift_target = self.bipolar() * self.wow_amp * 0.3;
        }
        self.drift += (self.drift_target - self.drift) * 0.0005;
        self.flut_ph += self.flut_inc;
        if self.flut_ph > TAU {
            self.flut_ph -= TAU;
        }
        // A warped record adds a second harmonic to the eccentricity wow.
        let d = self.base_delay
            + self.wow_amp * (self.rot_sin + 0.15 * (2.0 * rot + 1.3).sin())
            + self.flut_amp * self.flut_ph.sin()
            + self.drift;

        let base = self.base_delay as usize;
        let dry_idx = w.wrapping_sub(base) & DELAY_MASK;
        let dry = (self.dl[0][dry_idx], self.dl[1][dry_idx]);

        let p = self.p;
        if p.wet <= 0.0 {
            self.w = (w + 1) & DELAY_MASK;
            return dry;
        }

        let mut x = [
            Self::read_frac(&self.dl[0], w, d),
            Self::read_frac(&self.dl[1], w, d),
        ];

        // --- tone + stylus
        for i in 0..2 {
            let mut s = self.ch[i].hp.tick(x[i]);
            s = self.ch[i].shelf.tick(s);
            if self.sat_on {
                let g = self.ch[i].pre.tick(s);
                let g = self.stylus(g);
                s = self.ch[i].de.tick(g);
                s = self.ch[i].dc.hp(s);
            }
            s = self.ch[i].lp1.tick(s);
            s = self.ch[i].lp2.tick(s);
            x[i] = s;
        }

        // --- stereo: bass is always mono, as cut on a lathe
        let m = 0.5 * (x[0] + x[1]);
        let sd = self.side_hp.tick(0.5 * (x[0] - x[1])) * p.width;
        let (mut wl, mut wr) = (m + sd, m - sd);

        // --- surface
        if p.crackle > 0.0 {
            let (cl, cr) = self.crackle(prev);
            wl += cl;
            wr += cr;
        }
        if p.noise > 0.0 {
            let (nl, nr) = self.groove_noise();
            wl += nl;
            wr += nr;
        }

        self.w = (w + 1) & DELAY_MASK;
        let dw = 1.0 - p.wet;
        (dry.0 * dw + wl * p.wet, dry.1 * dw + wr * p.wet)
    }

    /// Latency introduced by the effect, in samples.
    pub fn latency(&self) -> usize {
        self.base_delay as usize
    }
}

/// Transparent below 0.9, then bends smoothly to a ceiling of 1.0, so clicks on top of
/// a loud master never hard-clip in the codec or the speaker.
#[inline(always)]
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = x.abs();
    if a <= KNEE {
        return x;
    }

    let y = KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh();
    y.copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(v: &mut Vinyl, n: usize, mut f: impl FnMut(usize) -> f32) -> Vec<(f32, f32)> {
        (0..n).map(|i| v.tick(f(i), f(i))).collect()
    }

    /// Params with every effect off except the given overrides.
    fn clean(overrides: &[(&str, f32)]) -> Params {
        let p = Params::new();
        for (k, val) in [
            ("wet", 1.0),
            ("age", 0.0),
            ("crackle", 0.0),
            ("noise", 0.0),
            ("wow", 0.0),
            ("drive", 0.0),
            ("width", 1.0),
            ("rpm", 0.0),
        ] {
            p.set(k, val).unwrap();
        }
        for (k, val) in overrides {
            p.set(k, *val).unwrap();
        }
        p
    }

    /// Amplitude of `freq` in `x` (single-bin DFT).
    fn bin(x: &[f32], freq: f32, fs: f32) -> f32 {
        let (mut re, mut im) = (0f64, 0f64);
        for (i, &s) in x.iter().enumerate() {
            let ph = 2.0 * std::f64::consts::PI * freq as f64 * i as f64 / fs as f64;
            re += s as f64 * ph.cos();
            im += s as f64 * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// (H2, H3) relative to the fundamental, in dB, for a sine of `amp` at `freq`.
    fn harmonics(p: &Params, freq: f32, amp: f32) -> (f32, f32) {
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        v.update(p);
        let out = run(&mut v, 48_000, |i| (TAU * freq * i as f32 / fs).sin() * amp);
        let tail: Vec<f32> = out[24_000..].iter().map(|s| s.0).collect();
        let h1 = bin(&tail, freq, fs);
        let db = |h: f32| 20.0 * (h / h1).max(1e-9).log10();
        (db(bin(&tail, 2.0 * freq, fs)), db(bin(&tail, 3.0 * freq, fs)))
    }

    fn rms(x: impl Iterator<Item = f32>) -> f32 {
        let (mut s, mut n) = (0f64, 0usize);
        for v in x {
            s += (v as f64) * (v as f64);
            n += 1;
        }
        (s / n as f64).sqrt() as f32
    }

    #[test]
    fn dry_is_pure_delay() {
        let p = Params::new();
        p.set("wet", 0.0).unwrap();
        let mut v = Vinyl::new(48_000.0);
        v.update(&p);
        let lat = v.latency();
        let out = run(&mut v, 2000, |i| (i as f32 * 0.01).sin());
        for i in lat..2000 {
            let expect = ((i - lat) as f32 * 0.01).sin();
            assert!((out[i].0 - expect).abs() < 1e-5, "i={i}");
        }
    }

    #[test]
    fn stable_and_bounded_for_all_presets() {
        for (name, _, _) in crate::params::PRESETS {
            let p = Params::new();
            p.apply_preset(name).unwrap();
            for sr in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
                let mut v = Vinyl::new(sr);
                v.update(&p);
                let mut rng = 1u32;
                let out = run(&mut v, (sr as usize) * 2, |_| {
                    rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (rng >> 8) as f32 / 8_388_608.0 - 1.0
                });
                for (l, r) in out {
                    assert!(l.is_finite() && r.is_finite(), "{name}@{sr}");
                    assert!(l.abs() < 4.0 && r.abs() < 4.0, "{name}@{sr}: {l} {r}");
                }
            }
        }
    }

    fn rms_of_sine(age: f32, freq: f32) -> f32 {
        let p = clean(&[("age", age)]);
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        v.update(&p);
        let out = run(&mut v, 48_000, |i| (TAU * freq * i as f32 / fs).sin() * 0.5);
        rms(out[24_000..].iter().map(|s| s.0))
    }

    #[test]
    fn age_rolls_off_highs_not_mids() {
        let hi_new = rms_of_sine(0.0, 12_000.0);
        let hi_old = rms_of_sine(1.0, 12_000.0);
        let mid_old = rms_of_sine(1.0, 1_000.0);
        assert!(
            hi_old < hi_new * 0.1,
            "12k should be heavily cut: {hi_old} vs {hi_new}"
        );
        assert!(mid_old > 0.25, "1k should pass: {mid_old}");
    }

    #[test]
    fn emphasis_shelves_cancel() {
        let fs = 48_000.0;
        let (mut pre, mut de) = (Biquad::default(), Biquad::default());
        pre.highshelf(fs, PRE_EMPH_HZ, PRE_EMPH_DB);
        de.highshelf(fs, PRE_EMPH_HZ, -PRE_EMPH_DB);
        for f in [100.0, 2_000.0, 10_000.0] {
            let out: Vec<f32> = (0..24_000)
                .map(|i| de.tick(pre.tick((TAU * f * i as f32 / fs).sin())))
                .collect();
            let a = bin(&out[12_000..], f, fs);
            assert!((a - 1.0).abs() < 0.01, "{f} Hz: {a}");
        }
    }

    #[test]
    fn stylus_distortion_is_mostly_even_order_and_treble_weighted() {
        let p = clean(&[("drive", 0.35)]);
        let (h2, h3) = harmonics(&p, 1_000.0, 0.5);
        assert!(h2 > h3, "2nd harmonic should dominate: h2={h2} h3={h3}");
        assert!(h2 < -30.0 && h2 > -60.0, "1 kHz h2 {h2} dB");
        let (h2_hi, _) = harmonics(&p, 4_000.0, 0.5);
        assert!(h2_hi > h2, "treble should distort more: {h2_hi} vs {h2}");
        let (h2_off, h3_off) = harmonics(&clean(&[]), 1_000.0, 0.5);
        assert!(h2_off < -90.0 && h3_off < -90.0, "drive 0 is clean");
    }

    #[test]
    fn bass_is_mono() {
        let p = clean(&[]);
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        v.update(&p);
        // Opposite-polarity 50 Hz: a pure side signal.
        let out: Vec<(f32, f32)> = (0..48_000)
            .map(|i| {
                let s = (TAU * 50.0 * i as f32 / fs).sin() * 0.5;
                v.tick(s, -s)
            })
            .collect();
        let side = rms(out[24_000..].iter().map(|(l, r)| 0.5 * (l - r)));
        assert!(side < 0.05, "50 Hz side should be mostly removed: {side}");
    }

    #[test]
    fn crackle_produces_impulses_on_silence() {
        let p = Params::new();
        p.apply_preset("worn").unwrap();
        p.set("noise", 0.0).unwrap();
        let mut v = Vinyl::new(48_000.0);
        v.update(&p);
        let out = run(&mut v, 48_000 * 3, |_| 0.0);
        let peak = out
            .iter()
            .fold(0f32, |m, (l, r)| m.max(l.abs()).max(r.abs()));
        assert!(peak > 0.005, "expected audible clicks, peak={peak}");
        assert!(peak < 1.0, "clicks too loud, peak={peak}");
    }

    #[test]
    fn scratches_repeat_every_revolution() {
        let fs = 48_000.0;
        let p = clean(&[("crackle", 1.0)]);
        let mut v = Vinyl::new(fs as f64);
        v.update(&p);
        // Only scratches: no random dust.
        v.tick_p = 0.0;
        v.pop_p = 0.0;
        v.fizz_p = 0.0;
        let rev = (fs / ROTATION_HZ[0]) as usize;
        let out = run(&mut v, rev * 12, |_| 0.0);
        let env: Vec<f32> = out.iter().map(|(l, r)| l.abs() + r.abs()).collect();

        // Energy at a lag of one revolution vs. half a revolution.
        let corr = |lag: usize| -> f32 {
            (rev..env.len() - lag).map(|i| env[i] * env[i + lag]).sum()
        };
        let (one, half) = (corr(rev), corr(rev / 2));
        assert!(one > 0.0, "scratches should fire");
        assert!(one > half * 5.0, "one rev {one} vs half {half}");
    }

    #[test]
    fn noise_level_follows_the_knob() {
        let fs = 48_000.0;
        let level = |noise: f32| {
            let mut v = Vinyl::new(fs as f64);
            v.update(&clean(&[("noise", noise)]));
            let out = run(&mut v, 96_000, |_| 0.0);
            20.0 * rms(out[48_000..].iter().map(|s| s.0)).log10()
        };
        let (lo, hi) = (level(0.3), level(1.0));
        assert!((lo - -63.0).abs() < 3.0, "noise 0.3: {lo} dBFS");
        assert!((hi - -42.0).abs() < 3.0, "noise 1.0: {hi} dBFS");
    }

    #[test]
    fn nothing_generated_below_the_speaker_range() {
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        v.update(&clean(&[("noise", 1.0)]));
        let out = run(&mut v, 96_000, |_| 0.0);
        let (mut a, mut b) = (Biquad::default(), Biquad::default());
        a.lowpass(fs, 60.0, 0.707);
        b.lowpass(fs, 60.0, 0.707);
        let total = rms(out[48_000..].iter().map(|s| s.0));
        let low = rms(out.iter().map(|s| b.tick(a.tick(s.0))).skip(48_000));
        assert!(low < total * 0.05, "below 60 Hz {low} vs total {total}");
    }

    #[test]
    fn soft_clip_is_transparent_then_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.9), -0.9);
        assert!(soft_clip(1.5) <= 1.0 && soft_clip(1.5) > 0.95);
        assert!(soft_clip(-4.0) >= -1.0);
        assert!(soft_clip(0.95) < 0.95 && soft_clip(0.95) > 0.93);
    }

    #[test]
    #[ignore = "prints measurements for tuning"]
    fn report() {
        for (name, _, _) in crate::params::PRESETS {
            let p = Params::new();
            p.apply_preset(name).unwrap();
            p.set("wow", 0.0).unwrap();
            let (h2, h3) = harmonics(&p, 1_000.0, 0.5);
            let (h2h, h3h) = harmonics(&p, 6_000.0, 0.25);
            println!("{name:>8}: 1k@-6dB h2 {h2:6.1} h3 {h3:6.1} | 6k@-12dB h2 {h2h:6.1} h3 {h3h:6.1}");
        }
    }

    #[test]
    fn noise_generator_rms() {
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        let mut sh = NoiseShaper::default();
        sh.set(fs);
        let x: Vec<f32> = (0..200_000).map(|_| {
            let w = v.bipolar();
            sh.shape(w)
        }).collect();
        let r = rms(x[10_000..].iter().copied());
        assert!((r - NOISE_GEN_RMS).abs() / NOISE_GEN_RMS < 0.1, "generator rms {r}");
    }
}
