//! One-off: prints the Vinylizer preset table from the calibrated V2 model, so every preset
//! shares the real-record calibration. Paste the output into `params::PRESETS`.

use vinylizer::params::{v2_values, V2Style, PARAM_NAMES};

fn main() {
    // key, label ru, label en, condition, era, color, scratches, width, gain, rpm, comment
    let presets: [(&str, &str, &str, f32, f32, f32, f32, f32, f32, f32, &str); 9] = [
        ("new", "Новая пластинка", "New record", 0.0, 1.0, 0.0, 0.0, 0.95, 1.0, 0.0, ""),
        ("subtle", "Чуть-чуть", "Subtle", 0.2, 1.0, 0.2, 0.05, 0.9, 1.0, 0.0, ""),
        ("warm", "Тёплая", "Warm", 0.4, 0.6, 0.35, 0.15, 0.85, 1.0, 0.0, "the calibrated record, 1972"),
        ("worn", "Заигранная", "Worn", 0.8, 0.6, 0.5, 0.45, 0.8, 1.0, 0.0, ""),
        ("single", "Сингл 45", "45 single", 0.35, 0.5, 0.3, 0.2, 0.85, 1.0, 1.0, "1965, 45 rpm"),
        ("shellac", "Шеллак 78", "Shellac 78", 1.0, 0.2, 0.7, 0.5, 0.0, 1.1, 2.0, "1944, 78 rpm, mono"),
        ("jazz", "Джаз 50-х", "1950s jazz", 0.55, 0.4, 0.4, 0.1, 0.9, 1.0, 0.0, "1958"),
        ("sinatra", "Синатра 50-х", "Sinatra 50s", 0.5, 0.37, 0.4, 0.1, 0.85, 1.0, 0.0, "1956"),
        ("jazz78", "Старый джаз 78", "Old jazz 78", 0.9, 0.1, 0.6, 0.35, 0.0, 1.05, 2.0, "1937, 78 rpm, mono"),
    ];
    let idx = |name: &str| PARAM_NAMES.iter().position(|n| *n == name).unwrap();

    for (key, ru, en, condition, era, color, scratches, width, gain, rpm, comment) in presets {
        let mut v = v2_values(condition, 0.5, color, era, V2Style::Plain);
        v[idx("scratches")] = scratches;
        v[idx("width")] = width;
        v[idx("gain")] = gain;
        v[idx("rpm")] = rpm;

        if !comment.is_empty() {
            println!("    // {comment}");
        }
        let vals: Vec<String> = v.iter().map(|x| format!("{:.3}", x)).collect();
        println!("    (\"{key}\", L::new(\"{ru}\", \"{en}\"), [{}]),", vals.join(", "));
    }
}
