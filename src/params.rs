//! Lock-free parameter block shared between the control thread and the
//! real-time audio thread. Writers bump `version`; the RT side re-derives
//! filter coefficients only when the version changes (once per block).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    #[inline]
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
    /// Returns the current value and resets to 0 (used for peak meters).
    #[inline]
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Relaxed))
    }
    /// Monotonic max for non-negative values (peak meters).
    #[inline]
    pub fn max(&self, v: f32) {
        self.0.fetch_max(v.to_bits(), Relaxed); // valid for v >= 0: IEEE bits order == value order
    }
}

/// Built-in effect parameters, all normalised to 0..1 unless noted.
#[derive(Debug)]
pub struct Params {
    pub wet: AtomicF32,     // dry/wet mix
    pub age: AtomicF32,     // HF roll-off + low-mid warmth
    pub crackle: AtomicF32, // clicks & pops density/level
    pub wow: AtomicF32,     // wow/flutter depth
    pub noise: AtomicF32,   // surface hiss + motor rumble
    pub drive: AtomicF32,   // soft saturation
    pub width: AtomicF32,   // stereo width (0 = mono, 1 = original)
    pub gain: AtomicF32,    // output gain, linear (0..2)
    pub bypass: AtomicBool,
    version: AtomicU32,
}

pub const PARAM_NAMES: &[&str] = &[
    "wet", "age", "crackle", "wow", "noise", "drive", "width", "gain",
];

pub const PRESETS: &[(&str, [f32; 8])] = &[
    //            wet   age   crackle wow   noise drive width gain
    ("subtle", [1.0, 0.30, 0.15, 0.15, 0.20, 0.20, 0.90, 1.0]),
    ("warm", [1.0, 0.45, 0.25, 0.25, 0.30, 0.35, 0.85, 1.0]),
    ("worn", [1.0, 0.60, 0.60, 0.40, 0.50, 0.45, 0.80, 1.0]),
    ("shellac", [1.0, 0.95, 0.80, 0.50, 0.70, 0.60, 0.00, 1.1]),
    // Derived from the user's iZotope Vinyl presets (Year / Wear / Dust / Scratch / Warp / Noise / Mono):
    // Jazz: 1950s, wear 25 %, dust 40 % loud, light scratches, no warp, quiet motor, stereo
    ("jazz", [1.0, 0.40, 0.30, 0.05, 0.20, 0.25, 0.90, 1.0]),
    // Sinatra 50s: 1950s, wear 30 %, softer dust, a touch more motor noise
    ("sinatra", [1.0, 0.45, 0.22, 0.05, 0.25, 0.30, 0.85, 1.0]),
    // Old 78 Jazz: 1930s 78 rpm, wear 40 %, dust 50 %, more scratches, slight warp, mono
    ("jazz78", [1.0, 0.90, 0.50, 0.15, 0.50, 0.45, 0.00, 1.05]),
];

impl Params {
    pub fn new() -> Self {
        let p = Self {
            wet: AtomicF32::new(0.0),
            age: AtomicF32::new(0.0),
            crackle: AtomicF32::new(0.0),
            wow: AtomicF32::new(0.0),
            noise: AtomicF32::new(0.0),
            drive: AtomicF32::new(0.0),
            width: AtomicF32::new(1.0),
            gain: AtomicF32::new(1.0),
            bypass: AtomicBool::new(false),
            version: AtomicU32::new(1),
        };
        p.apply_preset("warm").expect("built-in preset");
        p
    }

    fn slot(&self, name: &str) -> Option<&AtomicF32> {
        Some(match name {
            "wet" => &self.wet,
            "age" => &self.age,
            "crackle" => &self.crackle,
            "wow" => &self.wow,
            "noise" => &self.noise,
            "drive" => &self.drive,
            "width" => &self.width,
            "gain" => &self.gain,
            _ => return None,
        })
    }

    pub fn get(&self, name: &str) -> Option<f32> {
        self.slot(name).map(AtomicF32::get)
    }

    pub fn set(&self, name: &str, v: f32) -> Result<(), String> {
        let max = if name == "gain" { 2.0 } else { 1.0 };
        let slot = self.slot(name).ok_or_else(|| {
            format!(
                "unknown parameter '{name}' (have: {})",
                PARAM_NAMES.join(", ")
            )
        })?;
        if !v.is_finite() {
            return Err("value must be a number".into());
        }
        slot.set(v.clamp(0.0, max));
        self.version.fetch_add(1, Relaxed);
        Ok(())
    }

    pub fn apply_preset(&self, name: &str) -> Result<(), String> {
        let (_, vals) = PRESETS.iter().find(|(n, _)| *n == name).ok_or_else(|| {
            format!(
                "unknown preset '{name}' (have: {})",
                PRESETS.iter().map(|p| p.0).collect::<Vec<_>>().join(", ")
            )
        })?;
        for (n, v) in PARAM_NAMES.iter().zip(vals) {
            self.set(n, *v)?;
        }
        Ok(())
    }

    #[inline]
    pub fn version(&self) -> u32 {
        self.version.load(Relaxed)
    }

    pub fn describe(&self) -> String {
        let mut s = PARAM_NAMES
            .iter()
            .map(|n| format!("{n}={:.2}", self.get(n).unwrap()))
            .collect::<Vec<_>>()
            .join("  ");
        if self.bypass.load(Relaxed) {
            s.push_str("  [BYPASS]");
        }
        s
    }
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clamps_and_bumps_version() {
        let p = Params::new();
        let v0 = p.version();
        p.set("age", 3.0).unwrap();
        assert_eq!(p.get("age"), Some(1.0));
        assert!(p.version() > v0);
        assert!(p.set("nope", 0.1).is_err());
        assert!(p.set("age", f32::NAN).is_err());
    }

    #[test]
    fn atomic_max_and_take() {
        let a = AtomicF32::new(0.0);
        a.max(0.3);
        a.max(0.1);
        assert_eq!(a.take(), 0.3);
        assert_eq!(a.get(), 0.0);
    }
}
