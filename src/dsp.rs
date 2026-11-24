//! Built-in vinyl emulation. Real-time safe: all memory is allocated in
//! `Vinyl::new`, `tick` does no allocation, locking or syscalls.
//!
//! Signal chain (per channel):
//!
//! ```text
//! modulated delay (wow / flutter / drift)
//!   -> HP 28 Hz -> low-shelf warmth -> 2x LP (age-dependent cutoff)
//!   -> soft saturation -> stereo width
//!   + crackle (damped-sine ticks, low pops, surface fizz), hiss, motor rumble
//!   -> dry/wet (dry is read from the same delay line, so no comb filtering)
//! ```

use crate::params::Params;
use std::f32::consts::PI;

const DELAY_LEN: usize = 8192; // power of two; ~43 ms @ 192 kHz
const DELAY_MASK: usize = DELAY_LEN - 1;
const BASE_DELAY_MS: f32 = 3.0;
const DENORM: f32 = 1.0e-18;

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
}

const VOICES: usize = 8;

/// One ringing click: complex phasor rotating by (cr, ci) = decay * e^{iω}.
#[derive(Clone, Copy, Default)]
struct Voice {
    re: f32,
    im: f32,
    cr: f32,
    ci: f32,
    pan: f32,
}

#[derive(Clone, Copy, Default)]
struct Chan {
    hp: Biquad,
    shelf: Biquad,
    lp1: Biquad,
    lp2: Biquad,
    hiss: OnePole,
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
}

pub struct Vinyl {
    fs: f32,
    version: u32,
    p: Snapshot,
    ch: [Chan; 2],
    dl: [Box<[f32]>; 2],
    w: usize,
    base_delay: f32,
    // modulation
    wow_ph: f32,
    wow_inc: f32,
    flut_ph: f32,
    flut_inc: f32,
    wow_amp: f32,  // samples
    flut_amp: f32, // samples
    drift: f32,
    drift_target: f32,
    drift_ctr: u32,
    // saturation
    sat_k: f32,
    sat_norm: f32,
    // noise sources
    rng: u32,
    voices: [Voice; VOICES],
    next_voice: usize,
    tick_p: f32,
    pop_p: f32,
    fizz_p: f32,
    click_lvl: f32,
    hiss_lvl: f32,
    rumble: [OnePole; 2],
    rumble_lvl: f32,
}

impl Vinyl {
    pub fn new(sample_rate: f64) -> Self {
        let fs = sample_rate as f32;
        let mut v = Self {
            fs,
            version: 0,
            p: Snapshot::default(),
            ch: [Chan::default(); 2],
            dl: [
                vec![0.0; DELAY_LEN].into_boxed_slice(),
                vec![0.0; DELAY_LEN].into_boxed_slice(),
            ],
            w: 0,
            base_delay: BASE_DELAY_MS * fs / 1000.0,
            wow_ph: 0.0,
            wow_inc: 2.0 * PI * 0.555 / fs, // 33⅓ rpm eccentricity
            flut_ph: 0.0,
            flut_inc: 2.0 * PI * 6.5 / fs,
            wow_amp: 0.0,
            flut_amp: 0.0,
            drift: 0.0,
            drift_target: 0.0,
            drift_ctr: 0,
            sat_k: 1.0,
            sat_norm: 1.0,
            rng: 0x9E37_79B9,
            voices: [Voice::default(); VOICES],
            next_voice: 0,
            tick_p: 0.0,
            pop_p: 0.0,
            fizz_p: 0.0,
            click_lvl: 0.0,
            hiss_lvl: 0.0,
            rumble: [OnePole::default(); 2],
            rumble_lvl: 0.0,
        };
        for r in &mut v.rumble {
            r.set_lp(fs, 35.0);
        }
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
        };
        self.recompute();
    }

    fn recompute(&mut self) {
        let fs = self.fs;
        let p = self.p;
        let cutoff = 18_000.0 * (5_000.0f32 / 18_000.0).powf(p.age.powf(1.2));
        for c in &mut self.ch {
            c.hp.highpass(fs, 28.0, 0.707);
            c.shelf.lowshelf(fs, 220.0, 3.0 * p.age);
            c.lp1.lowpass(fs, cutoff, 0.6);
            c.lp2.lowpass(fs, cutoff * 1.15, 0.75);
            c.hiss.set_lp(fs, 7_000.0);
        }
        // peak pitch deviation: wow up to 0.45 %, flutter up to 0.08 %
        let sps = fs;
        self.wow_amp = p.wow * 0.0045 / (2.0 * PI * 0.555) * sps;
        self.flut_amp = p.wow * 0.0008 / (2.0 * PI * 6.5) * sps;
        // saturation: tanh(kx)/k keeps unity small-signal gain
        self.sat_k = 1.0 + p.drive * 3.0;
        self.sat_norm = 1.0 / self.sat_k;
        // crackle: ticks up to ~25/s, pops up to ~0.4/s, dense quiet surface fizz
        let c = p.crackle;
        self.tick_p = (c.powf(1.5) * 25.0) / fs;
        self.pop_p = (c * 0.4) / fs;
        self.fizz_p = (c * 300.0) / fs;
        self.click_lvl = 0.12 * c;
        self.hiss_lvl = 0.006 * p.noise;
        self.rumble_lvl = 0.35 * p.noise;
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

    /// Start a damped sinusoid: needle hitting a dust particle rings the
    /// stylus/cartridge briefly at a random resonance.
    #[inline(always)]
    fn trigger(&mut self, amp: f32, f_lo: f32, f_hi: f32, tau_lo: f32, tau_hi: f32, pan: f32) {
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
            pan,
        };
    }

    /// Returns (left, right) crackle contribution.
    #[inline(always)]
    fn crackle(&mut self) -> (f32, f32) {
        let u = self.rand();
        let lvl = self.click_lvl;
        if u < self.pop_p {
            let a = lvl * (1.0 + self.rand());
            let pan = self.bipolar() * 0.2;
            self.trigger(a, 250.0, 900.0, 0.0006, 0.0015, pan);
        } else if u < self.pop_p + self.tick_p {
            // log-distributed amplitude over ~30 dB: mostly faint, occasionally sharp
            let a = lvl * 10f32.powf(-1.5 * self.rand());
            let pan = self.bipolar() * 0.6;
            self.trigger(a, 1_800.0, 6_000.0, 0.00005, 0.0002, pan);
        } else if u < self.pop_p + self.tick_p + self.fizz_p {
            let a = lvl * 0.04 * self.rand();
            let pan = self.bipolar();
            self.trigger(a, 3_500.0, 9_000.0, 0.00002, 0.00006, pan);
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
            l += im * (1.0 - v.pan);
            r += im * (1.0 + v.pan);
        }
        (l, r)
    }

    /// Process one stereo frame.
    #[inline(always)]
    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let w = self.w;
        self.dl[0][w] = l;
        self.dl[1][w] = r;

        // --- modulation (slow random drift updated every 1024 samples)
        self.drift_ctr = self.drift_ctr.wrapping_add(1);
        if self.drift_ctr & 1023 == 0 {
            self.drift_target = self.bipolar() * self.wow_amp * 0.3;
        }
        self.drift += (self.drift_target - self.drift) * 0.0005;
        self.wow_ph += self.wow_inc;
        if self.wow_ph > 2.0 * PI {
            self.wow_ph -= 2.0 * PI;
        }
        self.flut_ph += self.flut_inc;
        if self.flut_ph > 2.0 * PI {
            self.flut_ph -= 2.0 * PI;
        }
        let d = self.base_delay
            + self.wow_amp * self.wow_ph.sin()
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

        // --- tone + saturation
        let drive = p.drive > 0.01;
        for (i, c) in self.ch.iter_mut().enumerate() {
            let mut s = c.hp.tick(x[i]);
            s = c.shelf.tick(s);
            s = c.lp1.tick(s);
            s = c.lp2.tick(s);
            if drive {
                s = (self.sat_k * s).tanh() * self.sat_norm;
            }
            x[i] = s;
        }

        // --- stereo width (M/S)
        let m = 0.5 * (x[0] + x[1]);
        let sd = 0.5 * (x[0] - x[1]) * p.width;
        let (mut wl, mut wr) = (m + sd, m - sd);

        // --- surface noise
        if p.crackle > 0.0 {
            let (cl, cr) = self.crackle();
            wl += cl;
            wr += cr;
        }
        if p.noise > 0.0 {
            let (n0, n1) = (self.bipolar(), self.bipolar());
            let h0 = self.ch[0].hiss.lp(n0);
            let h1 = self.ch[1].hiss.lp(n1);
            let rn = self.bipolar();
            let r0 = self.rumble[0].lp(rn);
            let rr = self.rumble[1].lp(r0);
            wl += h0 * self.hiss_lvl + rr * self.rumble_lvl;
            wr += h1 * self.hiss_lvl + rr * self.rumble_lvl;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(v: &mut Vinyl, n: usize, mut f: impl FnMut(usize) -> f32) -> Vec<(f32, f32)> {
        (0..n).map(|i| v.tick(f(i), f(i))).collect()
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
        for (name, _) in crate::params::PRESETS {
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
        let p = Params::new();
        for (k, val) in [
            ("wet", 1.0),
            ("age", age),
            ("crackle", 0.0),
            ("noise", 0.0),
            ("wow", 0.0),
            ("drive", 0.0),
            ("width", 1.0),
        ] {
            p.set(k, val).unwrap();
        }
        let fs = 48_000.0;
        let mut v = Vinyl::new(fs as f64);
        v.update(&p);
        let out = run(&mut v, 48_000, |i| {
            (2.0 * PI * freq * i as f32 / fs).sin() * 0.5
        });
        let tail = &out[24_000..];
        (tail.iter().map(|(l, _)| l * l).sum::<f32>() / tail.len() as f32).sqrt()
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
}
