//! Counts crackle events per second by peak level, to compare settings or versions:
//!
//! ```sh
//! cargo run --release --example surface_stats -- crackle=0.5 crackle_level=0.97 noise=0
//! ```
//!
//! Renders two minutes of silence through the built-in effect; unknown parameters are ignored.

use vinylizer::dsp::Vinyl;
use vinylizer::params::Params;

fn main() {
    let params = Params::new();
    for kv in std::env::args().skip(1) {
        if let Some((k, v)) = kv.split_once('=') {
            if let Ok(x) = v.parse::<f32>() {
                let _ = params.set(k, x);
            }
        }
    }

    let fs = 48_000.0;
    let secs = 120;
    let mut v = Vinyl::new(fs);
    v.update(&params);
    let env: Vec<f32> = (0..fs as usize * secs)
        .map(|_| {
            let (l, r) = v.tick(0.0, 0.0);
            l.abs().max(r.abs())
        })
        .collect();

    // An event is everything within 10 ms of crossing -80 dBFS; its level is its peak.
    let gap = (0.01 * fs) as usize;
    let mut events = Vec::new();
    let mut i = 0;
    while i < env.len() {
        if env[i] > 1e-4 {
            let end = (i + gap).min(env.len());
            events.push(env[i..end].iter().copied().fold(0f32, f32::max));
            i = end;
        } else {
            i += 1;
        }
    }

    let db = |x: f32| 20.0 * x.log10();

    // Timeline: loud events (≥ -20 dBFS) in each second of the first minute.
    if std::env::var("TIMELINE").is_ok() {
        let mut per_sec = vec![0u32; 60];
        let mut i = 0;
        while i < env.len().min(fs as usize * 60) {
            if db(env[i]) >= -20.0 {
                per_sec[i / fs as usize] += 1;
                i += gap;
            } else {
                i += 1;
            }
        }
        println!("loud events per second: {per_sec:?}");
    }

    print!("events/s by peak:");
    for th in [-40.0, -30.0, -20.0, -12.0, -6.0] {
        let n = events.iter().filter(|&&p| db(p) >= th).count();
        print!("  ≥{th:>4} dB: {:5.2}", n as f32 / secs as f32);
    }
    println!();
}
