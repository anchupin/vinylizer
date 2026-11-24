//! Lock-free parameter block shared between the control thread and the
//! real-time audio thread. Writers bump `version`; the RT side re-derives
//! filter coefficients only when the version changes (once per block).

use crate::i18n::L;
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
    pub age: AtomicF32,     // record wear: HF roll-off + low-mid warmth
    pub crackle: AtomicF32, // how much fine crackle: frequent faint ticks and fizz
    pub crackle_level: AtomicF32, // how loud the fine crackle is, see `crackle_peak_db`
    pub clicks: AtomicF32,  // how many distinct clicks and pops
    pub click_level: AtomicF32, // how loud clicks and scratches are, see `crackle_peak_db`
    pub scratches: AtomicF32, // how often scratches show up; each ticks once per revolution
    pub wow: AtomicF32,     // wow/flutter depth
    pub noise: AtomicF32,   // groove surface noise
    pub drive: AtomicF32,   // stylus distortion (even-order, treble-weighted)
    pub width: AtomicF32,   // stereo width (0 = mono, 1 = original)
    pub gain: AtomicF32,    // output gain, linear (0..2)
    pub rpm: AtomicF32,     // turntable speed index into `RPM_LABELS` (0..2)
    pub bypass: AtomicBool,
    /// Keep crackle and noise going while nothing plays (otherwise they fade out after a pause).
    pub idle_surface: AtomicBool,
    version: AtomicU32,
}

pub const PARAM_NAMES: &[&str] = &[
    "wet", "age", "crackle", "crackle_level", "clicks", "scratches", "click_level", "wow", "noise",
    "drive", "width", "gain", "rpm",
];

/// What each parameter is called in the UI and what it does, in the order of `PARAM_NAMES`.
pub const PARAM_INFO: &[(&str, L, L)] = &[
    ("wet", L::new("Сила эффекта", "Effect amount"),
        L::new("Сколько «пластинки» подмешано к оригиналу", "How much “record” is mixed into the original")),
    ("age", L::new("Износ пластинки", "Record wear"),
        L::new("Глушит верха: 0 — новая, 100% — заиграна", "Dulls the highs: 0 — new, 100% — worn out")),
    ("crackle", L::new("Треск", "Crackle"),
        L::new("Частое мелкое потрескивание — фон пластинки", "Frequent faint crackling — the record’s background")),
    ("crackle_level", L::new("Громкость треска", "Crackle level"),
        L::new("Насколько громок мелкий треск", "How loud the fine crackle is")),
    ("clicks", L::new("Щелчки", "Clicks"),
        L::new("Отдельные заметные щелчки и хлопки", "Distinct clicks and pops")),
    ("scratches", L::new("Царапины", "Scratches"),
        L::new("Как часто появляется щелчок, повторяющийся каждый оборот", "How often a click that repeats every revolution appears")),
    ("click_level", L::new("Громкость щелчков", "Click level"),
        L::new("Насколько громко щёлкают щелчки и царапины", "How loud clicks and scratches are")),
    ("wow", L::new("Плавание тона", "Wow & flutter"),
        L::new("Неровный ход диска и биение пластинки", "Uneven platter speed and record wobble")),
    ("noise", L::new("Шум поверхности", "Surface noise"),
        L::new("Шорох канавки, «дышит» в такт обороту", "Groove hiss that “breathes” with each revolution")),
    ("drive", L::new("Искажения иглы", "Stylus distortion"),
        L::new("Тёплая грязь на громких местах и шипящих", "Warm grit on loud parts and sibilants")),
    ("width", L::new("Ширина стерео", "Stereo width"),
        L::new("0 — моно, как у 78-х; бас моно всегда", "0 — mono, like 78s; bass is always mono")),
    ("gain", L::new("Громкость", "Volume"),
        L::new("Итоговый уровень; 0 dB — без изменений", "Output level; 0 dB — unchanged")),
    ("rpm", L::new("Скорость", "Speed"),
        L::new("Задаёт период щелчков и плавания", "Sets the period of scratches and wow")),
];

pub const RPM_LABELS: &[L] = &[
    L::new("33⅓ об/мин", "33⅓ rpm"),
    L::new("45 об/мин", "45 rpm"),
    L::new("78 об/мин", "78 rpm"),
];

/// What "reset" returns the built-in effect to.
pub const DEFAULT_PRESET: &str = "warm";

/// Peak level, dBFS, of the loudest crackle ticks or clicks for a level of 0..1 (−40 to 0 dBFS).
/// They are a fraction of a millisecond long, so even 0 dBFS sounds quieter than music at
/// that level; the output limiter keeps anything louder from clipping.
pub fn crackle_peak_db(level: f32) -> f32 {
    -40.0 + 40.0 * level
}

pub const PRESETS: &[(&str, L, [f32; 13])] = &[
    //                       wet   age   crack c.lvl click scrat k.lvl wow   noise drive width gain  rpm
    // crackle level: new −34 … shellac −18 dBFS; click level: new −28 … shellac −10 dBFS
    ("new", L::new("Новая пластинка", "New record"), [1.0, 0.15, 0.10, 0.15, 0.05, 0.00, 0.30, 0.10, 0.15, 0.20, 0.95, 1.0, 0.0]),
    ("subtle", L::new("Чуть-чуть", "Subtle"), [1.0, 0.30, 0.18, 0.25, 0.10, 0.05, 0.40, 0.15, 0.20, 0.25, 0.90, 1.0, 0.0]),
    ("warm", L::new("Тёплая", "Warm"), [1.0, 0.45, 0.30, 0.30, 0.20, 0.15, 0.50, 0.25, 0.30, 0.35, 0.85, 1.0, 0.0]),
    ("worn", L::new("Заигранная", "Worn"), [1.0, 0.60, 0.60, 0.45, 0.50, 0.45, 0.65, 0.40, 0.50, 0.50, 0.80, 1.0, 0.0]),
    ("single", L::new("Сингл 45", "45 single"), [1.0, 0.35, 0.30, 0.35, 0.25, 0.20, 0.55, 0.20, 0.25, 0.40, 0.85, 1.0, 1.0]),
    ("shellac", L::new("Шеллак 78", "Shellac 78"), [1.0, 0.95, 0.80, 0.55, 0.60, 0.50, 0.75, 0.50, 0.70, 0.60, 0.00, 1.1, 2.0]),
    // Derived from the user's iZotope Vinyl presets (Year / Wear / Dust / Scratch / Warp / Noise / Mono):
    // Jazz: 1950s, wear 25 %, dust 40 % loud, light scratches, no warp, quiet motor, stereo
    ("jazz", L::new("Джаз 50-х", "1950s jazz"), [1.0, 0.40, 0.35, 0.35, 0.25, 0.10, 0.55, 0.05, 0.20, 0.25, 0.90, 1.0, 0.0]),
    // Sinatra 50s: 1950s, wear 30 %, softer dust, a touch more motor noise
    ("sinatra", L::new("Синатра 50-х", "Sinatra 50s"), [1.0, 0.45, 0.28, 0.30, 0.18, 0.10, 0.50, 0.05, 0.25, 0.30, 0.85, 1.0, 0.0]),
    // Old 78 Jazz: 1930s 78 rpm, wear 40 %, dust 50 %, more scratches, slight warp, mono
    ("jazz78", L::new("Старый джаз 78", "Old jazz 78"), [1.0, 0.90, 0.55, 0.50, 0.45, 0.35, 0.70, 0.15, 0.50, 0.45, 0.00, 1.05, 2.0]),
];

impl Params {
    pub fn new() -> Self {
        let p = Self {
            wet: AtomicF32::new(0.0),
            age: AtomicF32::new(0.0),
            crackle: AtomicF32::new(0.0),
            crackle_level: AtomicF32::new(0.0),
            clicks: AtomicF32::new(0.0),
            click_level: AtomicF32::new(0.0),
            scratches: AtomicF32::new(0.0),
            wow: AtomicF32::new(0.0),
            noise: AtomicF32::new(0.0),
            drive: AtomicF32::new(0.0),
            width: AtomicF32::new(1.0),
            gain: AtomicF32::new(1.0),
            rpm: AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            idle_surface: AtomicBool::new(false),
            version: AtomicU32::new(1),
        };
        p.apply_preset(DEFAULT_PRESET).expect("built-in preset");
        p
    }

    fn slot(&self, name: &str) -> Option<&AtomicF32> {
        Some(match name {
            "wet" => &self.wet,
            "age" => &self.age,
            "crackle" => &self.crackle,
            "crackle_level" => &self.crackle_level,
            "clicks" => &self.clicks,
            "click_level" => &self.click_level,
            "scratches" => &self.scratches,
            "wow" => &self.wow,
            "noise" => &self.noise,
            "drive" => &self.drive,
            "width" => &self.width,
            "gain" => &self.gain,
            "rpm" => &self.rpm,
            _ => return None,
        })
    }

    pub fn get(&self, name: &str) -> Option<f32> {
        self.slot(name).map(AtomicF32::get)
    }

    pub fn set(&self, name: &str, v: f32) -> Result<(), String> {
        let slot = self.slot(name).ok_or_else(|| {
            format!(
                "unknown parameter '{name}' (have: {})",
                PARAM_NAMES.join(", ")
            )
        })?;
        if !v.is_finite() {
            return Err("value must be a number".into());
        }
        let v = match name {
            "gain" => v.clamp(0.0, 2.0),
            "rpm" => v.round().clamp(0.0, (RPM_LABELS.len() - 1) as f32),
            _ => v.clamp(0.0, 1.0),
        };
        slot.set(v);
        self.version.fetch_add(1, Relaxed);
        Ok(())
    }

    pub fn apply_preset(&self, name: &str) -> Result<(), String> {
        let (_, _, vals) = PRESETS.iter().find(|(n, _, _)| *n == name).ok_or_else(|| {
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

    /// Index into `PRESETS` of the preset the current values match, if any.
    pub fn matching_preset(&self) -> Option<usize> {
        PRESETS.iter().position(|(_, _, vals)| self.matches(vals))
    }

    /// Whether the current values equal `vals` (in `PARAM_NAMES` order).
    pub fn matches(&self, vals: &[f32]) -> bool {
        vals.len() == PARAM_NAMES.len()
            && PARAM_NAMES
                .iter()
                .zip(vals)
                .all(|(n, v)| (self.get(n).unwrap() - v).abs() < 0.005)
    }

    /// Current values in `PARAM_NAMES` order.
    pub fn values(&self) -> Vec<f32> {
        PARAM_NAMES.iter().map(|n| self.get(n).unwrap()).collect()
    }

    /// Set every parameter from `vals` (in `PARAM_NAMES` order).
    pub fn apply_values(&self, vals: &[f32]) {
        for (n, v) in PARAM_NAMES.iter().zip(vals) {
            let _ = self.set(n, *v);
        }
    }

    /// Speed index into `RPM_LABELS`.
    pub fn rpm_index(&self) -> usize {
        self.rpm.get() as usize
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
        p.set("rpm", 1.4).unwrap();
        assert_eq!(p.rpm_index(), 1);
        p.set("rpm", 9.0).unwrap();
        assert_eq!(p.rpm_index(), 2);
    }

    #[test]
    fn presets_cover_every_param_and_match_back() {
        assert_eq!(PARAM_INFO.len(), PARAM_NAMES.len());
        for ((k, _, _), n) in PARAM_INFO.iter().zip(PARAM_NAMES) {
            assert_eq!(k, n);
        }
        let p = Params::new();
        for (i, (name, _, _)) in PRESETS.iter().enumerate() {
            p.apply_preset(name).unwrap();
            assert_eq!(p.matching_preset(), Some(i), "{name}");
        }
        p.set("crackle", 0.123).unwrap();
        assert_eq!(p.matching_preset(), None);
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
