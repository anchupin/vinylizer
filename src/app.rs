//! Entry point: menu bar app by default, `--cli` for a terminal console.

use crate::au;
use crate::hal;
use crate::pipeline::Pipeline;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vinylizer::params::{Params, PARAM_NAMES, PRESETS};
use vinylizer::settings::Settings;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Relaxed);
}

const USAGE: &str = "\
vinylizer — real-time vinyl sound for macOS system audio

USAGE:
  vinylizer                 menu bar app (default)
  vinylizer --cli [OPTIONS] terminal mode

CLI OPTIONS:
  --app <bundle>       process only apps whose bundle id starts with <bundle>, repeatable
                       (e.g. com.spotify.client); default: all system audio
  --preset <name>      built-in preset: new | subtle | warm | worn | single | shellac | jazz | sinatra | jazz78
                       (default: warm)
  --set <k>=<v>        override a built-in parameter, repeatable
                       (wet age crackle scratches crackle_level wow noise drive width gain 0..2,
                       rpm 0=33⅓ 1=45 2=78)
  --au <name>          host an Audio Unit effect instead of the built-in DSP (e.g. \"Vinyl\")
  --chain              with --au: run the built-in DSP after the AU as well
  --list-devices       list output devices
  --list-apps          list processes known to Core Audio
  --list-plugins       list installed AU effects
  -h, --help";

pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Finder/LaunchServices may pass -psn_… ; anything else means CLI.
    let cli = args.iter().any(|a| !a.starts_with("-psn_"));
    hal::init();
    if !cli {
        crate::ui::run();
        return 0;
    }
    match run_cli(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run_cli(args: &[String]) -> Result<(), String> {
    let params = Arc::new(Params::new());
    let mut cfg = Settings::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{n} needs a value"))
        };
        match a.as_str() {
            "--cli" => {}
            "--app" => cfg.targets.push(val("--app")?),
            "--preset" => params.apply_preset(&val("--preset")?)?,
            "--au" => {
                cfg.au = Some(val("--au")?);
                cfg.mode = vinylizer::settings::Mode::Plugin;
            }
            "--chain" => cfg.chain = true,
            "--set" => {
                let s = val("--set")?;
                let (k, v) = s.split_once('=').ok_or("--set expects key=value")?;
                params.set(k, v.parse().map_err(|_| format!("bad value in --set {s}"))?)?;
            }
            "--list-devices" => {
                let def = hal::default_output().ok().map(|d| d.id);
                for d in hal::list_devices()?
                    .into_iter()
                    .filter(|d| d.out_channels > 0)
                {
                    println!(
                        "{} {:<40} {}ch  {}",
                        if Some(d.id) == def { "*" } else { " " },
                        d.name,
                        d.out_channels,
                        d.uid
                    );
                }
                return Ok(());
            }
            "--list-apps" => {
                for p in hal::list_processes()? {
                    println!(
                        "{} pid {:<7} {}",
                        if p.playing { "▶" } else { " " },
                        p.pid,
                        p.bundle
                    );
                }
                return Ok(());
            }
            "--list-plugins" => {
                for c in au::list_effects() {
                    println!("{:<48} {}", c.name, c.codes());
                }
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument '{other}'\n\n{USAGE}")),
        }
    }

    let mut pipe = Pipeline::new(params.clone());
    pipe.apply(&cfg)?;
    println!("▶ {}", pipe.status);
    println!("  {}", params.describe());
    println!("  Type `help` for commands, Ctrl-C to stop.");

    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    {
        let params = params.clone();
        let meter = pipe.meter.clone();
        let au = pipe.au().map(|a| a.handle());
        std::thread::spawn(move || console(params, meter, au));
    }
    let mut last_check = Instant::now();
    while !STOP.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
        if last_check.elapsed() > Duration::from_secs(2) {
            last_check = Instant::now();
            if pipe.needs_rebuild(&cfg) {
                match pipe.apply(&cfg) {
                    Ok(()) => println!("\n↻ {}", pipe.status),
                    Err(e) => eprintln!("\n! {e}"),
                }
            }
        }
    }
    drop(pipe);
    println!("\nstopped, audio routing restored");
    Ok(())
}

fn console(params: Arc<Params>, meter: Arc<crate::engine::Meter>, au: Option<au::AuHandle>) {
    let prompt = || {
        print!("vinyl> ");
        let _ = std::io::stdout().flush();
    };
    prompt();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let w: Vec<&str> = line.split_whitespace().collect();
        let r: Result<(), String> = match w.as_slice() {
            [] => Ok(()),
            ["help" | "?"] => {
                println!(
                    "  show | set <param> <0..1> | preset <{}> | bypass | meter\n  \
                     au | au set <id> <value> | au presets | au preset <n> | quit\n  params: {}",
                    PRESETS.iter().map(|p| p.0).collect::<Vec<_>>().join("|"),
                    PARAM_NAMES.join(" ")
                );
                Ok(())
            }
            ["show"] => {
                println!("  {}", params.describe());
                Ok(())
            }
            ["set", k, v] => v
                .parse::<f32>()
                .map_err(|_| "value must be a number".to_string())
                .and_then(|v| params.set(k, v)),
            ["preset", n] => params.apply_preset(n),
            ["bypass"] => {
                let b = !params.bypass.load(Relaxed);
                params.bypass.store(b, Relaxed);
                println!("  bypass {}", if b { "ON" } else { "off" });
                Ok(())
            }
            ["meter"] => {
                let db = |x: f32| if x > 0.0 { 20.0 * x.log10() } else { -120.0 };
                println!(
                    "  in {:>6.1} dBFS   out {:>6.1} dBFS   callbacks {}",
                    db(meter.in_peak.take()),
                    db(meter.out_peak.take()),
                    meter.callbacks.load(Relaxed)
                );
                Ok(())
            }
            ["au", rest @ ..] => match (au, rest) {
                (None, _) => Err("no AU loaded (start with --au <name>)".into()),
                (Some(h), []) => {
                    for p in h.params() {
                        println!(
                            "  {:>6}  {:<32} {:>10.3}   [{} .. {}]",
                            p.id, p.name, p.value, p.min, p.max
                        );
                    }
                    Ok(())
                }
                (Some(h), ["presets"]) => {
                    for (n, name) in h.factory_presets() {
                        println!("  {n:>4}  {name}");
                    }
                    Ok(())
                }
                (Some(h), ["preset", n]) => n
                    .parse()
                    .map_err(|_| "preset number expected".to_string())
                    .and_then(|n| h.set_preset(n)),
                (Some(h), ["set", id, v]) => match (id.parse::<u32>(), v.parse::<f32>()) {
                    (Ok(id), Ok(v)) => h.set_param(id, v),
                    _ => Err("usage: au set <id> <value>".into()),
                },
                _ => Err("unknown au command".into()),
            },
            ["quit" | "exit" | "q"] => {
                STOP.store(true, Relaxed);
                return;
            }
            _ => Err("unknown command, type `help`".into()),
        };
        if let Err(e) = r {
            println!("  ! {e}");
        }
        prompt();
    }
}
