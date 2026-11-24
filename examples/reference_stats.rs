//! Statistics of vinyl surface noise, comparable between a real recording and our model.
//!
//! ```sh
//! cargo run --release --example reference_stats -- file ~/Downloads/vinyl.wav
//! cargo run --release --example reference_stats -- model crackle=0.39 crackle_level=0.633 ...
//! ```
//!
//! A recording's absolute level depends on how it was captured, so everything is relative to
//! its own noise floor: the RMS of the quiet surface between clicks. Reported:
//! - noise spectrum per octave, relative to the 1 kHz octave;
//! - clicks per second louder than the floor by +12 … +48 dB, and the power-law slope α of
//!   that count (louder by 20 dB → 10^α times rarer);
//! - for clicks 24+ dB above the floor: median length (time above 10 % of the peak and 5× the
//!   noise RMS) and median brightness (zero-crossing frequency within the click).
//!
//! Reads 16/24-bit PCM and 32-bit float WAV, uses the first channel only, skips the first 2 s
//! (a needle drop). The model is rendered mono-compatible the same way (left channel).

use std::f32::consts::PI;
use vinylizer::dsp::Vinyl;
use vinylizer::params::Params;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (x, fs) = match args.first().map(String::as_str) {
        Some("file") => read_wav(args.get(1).expect("file path")),
        Some("model") => render_model(&args[1..]),
        _ => {
            eprintln!("usage: reference_stats file <path.wav> | model k=v ...");
            std::process::exit(2);
        }
    };

    let skip = (2.0 * fs) as usize;
    let x = &x[skip.min(x.len())..];
    report(x, fs);
}

/// First channel of a WAV file as f32, and its sample rate.
fn read_wav(path: &str) -> (Vec<f32>, f32) {
    let bytes = std::fs::read(path).expect("read wav");
    assert!(&bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE", "not a WAV file");

    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);

    let (mut fmt, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32_at(i + 4) as usize;
        let body = i + 8;
        if id == b"fmt " {
            fmt = u16_at(body);
            channels = u16_at(body + 2);
            rate = u32_at(body + 4);
            bits = u16_at(body + 14);
            if fmt == 0xFFFE {
                fmt = u16_at(body + 24); // WAVE_FORMAT_EXTENSIBLE: sub-format tag
            }
        } else if id == b"data" {
            let data = &bytes[body..(body + size).min(bytes.len())];
            let frame = channels as usize * bits as usize / 8;
            let samples = data
                .chunks_exact(frame)
                .map(|f| match (fmt, bits) {
                    (1, 16) => i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0,
                    (1, 24) => ((i32::from_le_bytes([0, f[0], f[1], f[2]])) >> 8) as f32 / 8_388_608.0,
                    (3, 32) => f32::from_le_bytes([f[0], f[1], f[2], f[3]]),
                    _ => panic!("unsupported WAV format {fmt}/{bits} bit"),
                })
                .collect();
            println!("file: {path}, {rate} Hz, {bits} bit, {channels} ch");
            return (samples, rate as f32);
        }
        i = body + size + (size & 1);
    }
    panic!("no data chunk");
}

/// Two minutes of the model on silence, left channel.
fn render_model(kv: &[String]) -> (Vec<f32>, f32) {
    let params = Params::new();
    for a in kv {
        if let Some((k, v)) = a.split_once('=') {
            if let Ok(x) = v.parse::<f32>() {
                let _ = params.set(k, x);
            }
        }
    }

    let fs = 48_000.0;
    let mut v = Vinyl::new(fs as f64);
    v.update(&params);
    let x = (0..fs as usize * 122).map(|_| v.tick(0.0, 0.0).0).collect();
    println!("model: {}", kv.join(" "));
    (x, fs)
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-12).log10()
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

fn report(x: &[f32], fs: f32) {
    let secs = x.len() as f32 / fs;

    // Noise floor: the median RMS over 10 ms windows ignores the clicks.
    let win = (0.01 * fs) as usize;
    let mut rms: Vec<f32> = x
        .chunks(win)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let floor = median(&mut rms);
    println!("length {secs:.0} s, noise floor {:.1} dBFS RMS", db(floor));

    // Noise spectrum per octave (Goertzel-free: band-pass each octave, take the median window RMS).
    print!("octaves re 1 kHz:");
    let mut ref_1k = 0.0;
    let mut bands = Vec::new();
    for f in [125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0] {
        if f > fs * 0.4 {
            continue;
        }
        let y = bandpass(x, fs, f);
        let mut r: Vec<f32> = y
            .chunks(win)
            .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
            .collect();
        let level = db(median(&mut r));
        if f == 1_000.0 {
            ref_1k = level;
        }
        bands.push((f, level));
    }
    for (f, level) in bands {
        print!("  {}k {:+.1}", f / 1000.0, level - ref_1k);
    }
    println!();

    // Clicks: everything within 5 ms of crossing floor + 12 dB is one event.
    let gap = (0.005 * fs) as usize;
    let start = floor * 10f32.powf(12.0 / 20.0);
    let mut events: Vec<(usize, f32)> = Vec::new();
    let mut i = 0;
    while i < x.len() {
        if x[i].abs() > start {
            let end = (i + gap).min(x.len());
            let (k, p) = x[i..end]
                .iter()
                .enumerate()
                .fold((0, 0f32), |(bk, bp), (k, s)| if s.abs() > bp { (k, s.abs()) } else { (bk, bp) });
            events.push((i + k, p));
            i = end;
        } else {
            i += 1;
        }
    }

    print!("clicks/s above floor:");
    let mut counts = Vec::new();
    for above in [12.0, 18.0, 24.0, 30.0, 36.0, 42.0, 48.0] {
        let th = floor * 10f32.powf(above / 20.0);
        let n = events.iter().filter(|e| e.1 >= th).count();
        counts.push((above, n));
        print!("  +{above:.0} {:.2}", n as f32 / secs);
    }
    println!();

    // Power-law slope between +18 and +36 dB (enough events, below the clipping region).
    let pick = |a: f32| counts.iter().find(|c| c.0 == a).map(|c| c.1 as f32).unwrap_or(0.0);
    let (n18, n36) = (pick(18.0), pick(36.0));
    if n18 > 0.0 && n36 > 0.0 {
        let alpha = (n18 / n36).log10() / (18.0 / 20.0);
        println!("power-law slope α ≈ {alpha:.2} (each +20 dB is {:.0}× rarer)", 10f32.powf(alpha));
    }

    // Shape of the clear clicks.
    let th = floor * 10f32.powf(24.0 / 20.0);
    let mut lens = Vec::new();
    let mut freqs = Vec::new();
    for &(k, p) in events.iter().filter(|e| e.1 >= th) {
        let lo = k.saturating_sub(gap);
        let hi = (k + 4 * gap).min(x.len());
        // Above 10 % of the peak, but clear of the noise (5× its RMS).
        let level = (0.1 * p).max(5.0 * floor);
        let above: Vec<usize> = (lo..hi).filter(|&j| x[j].abs() > level).collect();
        if let (Some(&a), Some(&b)) = (above.first(), above.last()) {
            lens.push((b - a + 1) as f32 / fs * 1e3);
            let zc = (a..b).filter(|&j| (x[j] >= 0.0) != (x[j + 1] >= 0.0)).count();
            freqs.push(zc as f32 / 2.0 / ((b - a + 1) as f32 / fs));
        }
    }
    if !lens.is_empty() {
        println!(
            "clicks +24 dB: {} of them, median length {:.2} ms, median brightness {:.0} Hz",
            lens.len(),
            median(&mut lens),
            median(&mut freqs)
        );
    }
}

/// One octave around `f`: two cascaded biquad band-passes (Q ≈ 1.4).
fn bandpass(x: &[f32], fs: f32, f: f32) -> Vec<f32> {
    let w = 2.0 * PI * f / fs;
    let alpha = w.sin() / (2.0 * 1.41);
    let a0 = 1.0 + alpha;
    let (b0, b2, a1, a2) = (alpha / a0, -alpha / a0, -2.0 * w.cos() / a0, (1.0 - alpha) / a0);
    let mut y = x.to_vec();
    for _ in 0..2 {
        let (mut x1, mut x2, mut y1, mut y2) = (0f32, 0f32, 0f32, 0f32);
        for s in y.iter_mut() {
            let v = b0 * *s + b2 * x2 - a1 * y1 - a2 * y2;
            x2 = x1;
            x1 = *s;
            y2 = y1;
            y1 = v;
            *s = v;
        }
    }
    y
}
