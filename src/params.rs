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
    pub year: AtomicF32,    // era of the recording and pressing, see `dsp::year_of`
    pub age: AtomicF32,     // record wear: HF roll-off + low-mid warmth
    pub crackle: AtomicF32, // how many dust particles per second (and fizz)
    pub crackle_level: AtomicF32, // background level of the quiet ticks, see `crackle_bed_db`
    pub clicks: AtomicF32,  // how heavy the tail is: share of loud clicks, see `dsp::loud_share`
    pub click_level: AtomicF32, // ceiling for clicks and scratches, see `crackle_peak_db`
    pub pops: AtomicF32,    // rare loud pops from big particles, see `dsp::pops_per_s`
    pub scratches: AtomicF32, // how often scratches show up; each ticks once per revolution
    pub wow: AtomicF32,     // wow/flutter depth
    pub noise: AtomicF32,   // groove surface noise
    pub surface_gain: AtomicF32, // moves crackle, clicks and noise together, see `surface_offset_db`
    pub drive: AtomicF32,   // stylus distortion (even-order, treble-weighted)
    pub color: AtomicF32,   // warm vintage tone: low-mid lift, softer highs, more harmonics
    pub width: AtomicF32,   // stereo width (0 = mono, 1 = original)
    pub gain: AtomicF32,    // output gain, linear (0..2)
    pub rpm: AtomicF32,     // turntable speed index into `RPM_LABELS` (0..2)
    pub bypass: AtomicBool,
    /// Keep crackle and noise going while nothing plays (otherwise they fade out after a pause).
    pub idle_surface: AtomicBool,
    version: AtomicU32,
}

pub const PARAM_NAMES: &[&str] = &[
    "wet", "year", "age", "crackle", "crackle_level", "clicks", "pops", "scratches", "click_level", "noise",
    "surface_gain", "wow", "drive", "color", "width", "gain", "rpm",
];

/// What each parameter is called in the UI and what it does, in the order of `PARAM_NAMES`.
pub const PARAM_INFO: &[(&str, L, L)] = &[
    ("wet", L::new("Сила эффекта", "Effect amount"),
        L::new("Сколько «пластинки» подмешано к оригиналу", "How much “record” is mixed into the original")),
    ("year", L::new("Эпоха", "Era"),
        L::new("Звук записи своего времени: у старых уже полоса и моно", "Sound of its time: older records are narrower and mono")),
    ("age", L::new("Износ пластинки", "Record wear"),
        L::new("С какой частоты глохнут верха: новая 18 кГц, заиграна 5", "Where the highs fade: new 18 kHz, worn out 5")),
    ("crackle", L::new("Треск", "Crackle"),
        L::new("Сколько пылинок в секунду; почти все — тихий фон", "Dust particles per second; nearly all are a quiet background")),
    ("crackle_level", L::new("Громкость фона", "Background level"),
        L::new("Уровень обычных тихих тиков", "Level of the usual quiet ticks")),
    ("clicks", L::new("Щелчки", "Clicks"),
        L::new("Какая доля пылинок щёлкает громко; чем громче, тем реже", "Share of particles that click loudly; the louder, the rarer")),
    ("pops", L::new("Хлопки", "Pops"),
        L::new("Редкие громкие хлопки от крупных частиц, отдельно от пыли", "Rare loud pops from big particles, apart from the dust")),
    ("scratches", L::new("Царапины", "Scratches"),
        L::new("Как часто появляется новая царапина; щёлкает раз в оборот", "How often a new scratch appears; it clicks every revolution")),
    ("click_level", L::new("Пик щелчков", "Click peak"),
        L::new("Громче этого не бывает ни щелчок, ни царапина", "No click or scratch gets louder than this")),
    ("noise", L::new("Шум поверхности", "Surface noise"),
        L::new("Средний уровень шороха канавки; «дышит» с оборотом", "Average groove hiss level; “breathes” with each revolution")),
    ("surface_gain", L::new("Громкость эффектов", "Effects volume"),
        L::new("Треск, щелчки, царапины и шум вместе, баланс сохраняется", "Crackle, clicks, scratches and noise together, keeping their balance")),
    ("wow", L::new("Плавание тона", "Wow & flutter"),
        L::new("Неровный ход и биение: отклонение высоты тона", "Uneven speed and wobble: pitch deviation")),
    ("drive", L::new("Искажения иглы", "Stylus distortion"),
        L::new("Тёплая грязь на громких местах и шипящих", "Warm grit on loud parts and sibilants")),
    ("color", L::new("Окраска", "Color"),
        L::new("Тёплая винтажная подача: больше низкой середины, мягче верха", "Warm vintage tone: fuller low mids, softer highs")),
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

/// Ceiling, dBFS, for clicks and scratches for a level of 0..1 (−40 to 0 dBFS).
/// They are a fraction of a millisecond long, so even 0 dBFS sounds quieter than music at
/// that level; the output limiter keeps anything louder from clipping.
pub fn crackle_peak_db(level: f32) -> f32 {
    -40.0 + 40.0 * level
}

/// Peak level, dBFS, of the usual quiet background ticks for a level of 0..1 (−55 to −25 dBFS).
pub fn crackle_bed_db(level: f32) -> f32 {
    -55.0 + 30.0 * level
}


/// V2: a record on a good 33⅓ turntable, tuned for the Marshall Emberton III, with two knobs
/// instead of fourteen. `condition` (0 new … 1 worn out) moves everything that wears together —
/// dust, clicks, groove hiss, dull treble, wow, stylus distortion — between three anchors;
/// `level` is the effects volume; `color` the warm vintage tone of the playback chain (music and
/// surface alike), which in V2 also adds a little wow and dulls the treble a touch.
/// No scratches; stereo with mono bass; the whole signal is the record.
///
/// The middle anchor (condition 0.4) is calibrated to a real LP's silent grooves (Freesound
/// #140295 by joedeshon, CC BY 4.0) with `examples/reference_stats.rs`, relative to the noise
/// floor: ~16 ticks/s, then 15.6 / 1.4 / 0.22 / 0.01 per second louder by +12 / +18 / +24 /
/// +30 dB, none beyond; clicks ring ~2 kHz for ~1.4 ms; pink hiss rolling off above 4 kHz.
/// On top of that, rare loud pops from big particles (about once in 40 s at the middle anchor),
/// which that clean record didn't have but played ones do.
pub fn v2_values(condition: f32, level: f32, color: f32, era: f32, style: V2Style) -> [f32; 17] {
    let color = color.clamp(0.0, 1.0);
    // Per anchor: age, crackle, crackle_level, clicks, click_level, noise, wow, drive, pops.
    const NEW: [f32; 9] = [0.20, 0.50, 0.30, 0.30, 0.80, 0.43, 0.06, 0.25, 0.08];
    const KEPT: [f32; 9] = [0.35, 0.74, 0.367, 0.45, 0.90, 0.533, 0.11, 0.35, 0.22];
    const WORN: [f32; 9] = [0.60, 0.95, 0.50, 0.70, 1.00, 0.70, 0.25, 0.50, 0.50];
    const KEPT_AT: f32 = 0.4;

    let c = condition.clamp(0.0, 1.0);
    let (a, b, t) = if c <= KEPT_AT {
        (NEW, KEPT, c / KEPT_AT)
    } else {
        (KEPT, WORN, (c - KEPT_AT) / (1.0 - KEPT_AT))
    };
    let m: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + (y - x) * t).collect();

    // Style: a jazz record is often an older, narrower and warmer pressing. Rock masters are
    // loud and dense and hide the surface, so rock lifts the background and the hiss by 6 dB
    // (the rare loud pops already reach the peak) and is brighter and wide.
    let (age_shift, width, surface_shift) = match style {
        V2Style::Plain => (0.0, 0.9, 0.0),
        V2Style::Jazz => (0.05, 0.7, 0.0),
        V2Style::Rock => (-0.05, 1.0, 0.2),
    };

    [
        1.0,                                                 // wet
        era.clamp(0.0, 1.0),                                 // year
        (m[0] + age_shift + 0.1 * color).clamp(0.0, 1.0),    // age
        m[1],                                                // crackle
        (m[2] + surface_shift).clamp(0.0, 1.0),              // crackle_level: +0.2 = +6 dB
        m[3],                                                // clicks
        m[8],                                                // pops
        0.0,                                                 // scratches: none
        m[4],                                                // click_level
        (m[5] + surface_shift).clamp(0.0, 1.0),              // noise: +0.2 = +6 dB
        level,                                               // surface_gain
        (m[6] + 0.15 * color).clamp(0.0, 1.0),               // wow: up to +0.07 %
        m[7],                                                // drive
        color,                                               // color
        width,                                               // width
        1.0,                                                 // gain
        0.0,                                                 // rpm: 33⅓
    ]
}

/// Where V2's color knob starts: audibly warm, still natural.
pub const V2_DEFAULT_COLOR: f32 = 0.5;

/// Where V2's era knob starts: 1972, a stereo record with a full band.
pub const V2_DEFAULT_ERA: f32 = 0.6;

/// V2 styles: where they put the two knobs, and what else they change (`v2_values`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2Style {
    Plain,
    /// Quiet, dynamic music where crackle shows anyway: a more played record, narrower stereo.
    Jazz,
    /// Loud, dense masters hide crackle: background and hiss +6 dB, a cleaner, brighter, wide record.
    Rock,
}

impl V2Style {
    pub const ALL: [V2Style; 3] = [V2Style::Plain, V2Style::Jazz, V2Style::Rock];

    pub fn label(self) -> L {
        match self {
            V2Style::Plain => L::new("Обычный", "Plain"),
            V2Style::Jazz => L::new("Джаз", "Jazz"),
            V2Style::Rock => L::new("Рок", "Rock"),
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            V2Style::Plain => "plain",
            V2Style::Jazz => "jazz",
            V2Style::Rock => "rock",
        }
    }

    pub fn from_code(code: &str) -> Option<V2Style> {
        V2Style::ALL.into_iter().find(|s| s.code() == code)
    }

    /// Where choosing the style puts the condition, effects-volume and era knobs.
    pub fn knobs(self) -> (f32, f32, f32) {
        match self {
            V2Style::Plain => (V2_DEFAULT_CONDITION, 0.5, V2_DEFAULT_ERA),
            V2Style::Jazz => (0.55, 0.5, 0.4),
            V2Style::Rock => (0.3, 0.5, 0.6),
        }
    }
}

/// Where V2's condition knob starts: the measured middle sound.
pub const V2_DEFAULT_CONDITION: f32 = 0.4;

/// V2 condition in words, for the value next to its slider.
pub fn v2_condition_label(condition: f32) -> L {
    match condition {
        c if c < 0.15 => L::new("новая", "new"),
        c if c < 0.55 => L::new("ухоженная", "well-kept"),
        c if c < 0.8 => L::new("поношенная", "played"),
        _ => L::new("заигранная", "worn out"),
    }
}

pub const PRESETS: &[(&str, L, [f32; 17])] = &[
    // Generated by `examples/gen_presets.rs` from the calibrated V2 model (record condition,
    // era, color), so every preset shares the real-record calibration; only scratches, stereo
    // width, gain and speed are set per preset.
    //                       wet   year  age   crack bed   click pops  scrat peak  noise surf  wow   drive color width gain  rpm
    ("new", L::new("Новая пластинка", "New record"), [1.000, 1.000, 0.200, 0.500, 0.300, 0.300, 0.080, 0.000, 0.800, 0.430, 0.500, 0.060, 0.250, 0.000, 0.950, 1.000, 0.000]),
    ("subtle", L::new("Чуть-чуть", "Subtle"), [1.000, 1.000, 0.295, 0.620, 0.334, 0.375, 0.150, 0.050, 0.850, 0.481, 0.500, 0.115, 0.300, 0.200, 0.900, 1.000, 0.000]),
    // the calibrated record, 1972
    ("warm", L::new("Тёплая", "Warm"), [1.000, 0.600, 0.385, 0.740, 0.367, 0.450, 0.220, 0.150, 0.900, 0.533, 0.500, 0.162, 0.350, 0.350, 0.850, 1.000, 0.000]),
    ("worn", L::new("Заигранная", "Worn"), [1.000, 0.600, 0.567, 0.880, 0.456, 0.617, 0.407, 0.450, 0.967, 0.644, 0.500, 0.278, 0.450, 0.500, 0.800, 1.000, 0.000]),
    // 1965, 45 rpm
    ("single", L::new("Сингл 45", "45 single"), [1.000, 0.500, 0.361, 0.710, 0.359, 0.431, 0.203, 0.200, 0.887, 0.520, 0.500, 0.149, 0.337, 0.300, 0.850, 1.000, 1.000]),
    // 1944, 78 rpm, mono
    ("shellac", L::new("Шеллак 78", "Shellac 78"), [1.000, 0.200, 0.670, 0.950, 0.500, 0.700, 0.500, 0.500, 1.000, 0.700, 0.500, 0.355, 0.500, 0.700, 0.000, 1.100, 2.000]),
    // 1958
    ("jazz", L::new("Джаз 50-х", "1950s jazz"), [1.000, 0.400, 0.452, 0.793, 0.400, 0.512, 0.290, 0.100, 0.925, 0.575, 0.500, 0.205, 0.387, 0.400, 0.900, 1.000, 0.000]),
    // 1956
    ("sinatra", L::new("Синатра 50-х", "Sinatra 50s"), [1.000, 0.370, 0.432, 0.775, 0.389, 0.492, 0.267, 0.100, 0.917, 0.561, 0.500, 0.193, 0.375, 0.400, 0.850, 1.000, 0.000]),
    // 1937, 78 rpm, mono
    ("jazz78", L::new("Старый джаз 78", "Old jazz 78"), [1.000, 0.100, 0.618, 0.915, 0.478, 0.658, 0.453, 0.350, 0.983, 0.672, 0.500, 0.317, 0.475, 0.600, 0.000, 1.050, 2.000]),
];

impl Params {
    pub fn new() -> Self {
        let p = Self {
            wet: AtomicF32::new(0.0),
            year: AtomicF32::new(1.0),
            age: AtomicF32::new(0.0),
            crackle: AtomicF32::new(0.0),
            crackle_level: AtomicF32::new(0.0),
            clicks: AtomicF32::new(0.0),
            click_level: AtomicF32::new(0.0),
            pops: AtomicF32::new(0.0),
            scratches: AtomicF32::new(0.0),
            wow: AtomicF32::new(0.0),
            noise: AtomicF32::new(0.0),
            surface_gain: AtomicF32::new(0.5),
            drive: AtomicF32::new(0.0),
            color: AtomicF32::new(0.0),
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
            "year" => &self.year,
            "age" => &self.age,
            "crackle" => &self.crackle,
            "crackle_level" => &self.crackle_level,
            "clicks" => &self.clicks,
            "click_level" => &self.click_level,
            "pops" => &self.pops,
            "scratches" => &self.scratches,
            "wow" => &self.wow,
            "noise" => &self.noise,
            "surface_gain" => &self.surface_gain,
            "drive" => &self.drive,
            "color" => &self.color,
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
    fn v2_condition_moves_wear_together_and_never_scratches() {
        let at = |c: f32| {
            let v = v2_values(c, 0.5, 0.0, 1.0, V2Style::Plain);
            move |name: &str| v[PARAM_NAMES.iter().position(|n| *n == name).unwrap()]
        };

        // The measured middle sound sits at the default condition.
        let kept = at(V2_DEFAULT_CONDITION);
        assert_eq!(kept("crackle"), 0.74);
        assert_eq!(kept("crackle_level"), 0.367);
        assert_eq!(kept("clicks"), 0.45);

        // Wear only adds dust, clicks and hiss and only dulls the treble; never scratches.
        let mut prev = at(0.0);
        for i in 1..=20 {
            let cur = at(i as f32 / 20.0);
            for name in ["age", "crackle", "crackle_level", "clicks", "pops", "click_level", "noise", "wow", "drive"] {
                assert!(cur(name) >= prev(name), "{name} must not drop with wear");
            }
            assert_eq!(cur("scratches"), 0.0);
            assert_eq!(cur("surface_gain"), 0.5);
            prev = cur;
        }
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
