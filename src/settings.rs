//! Persistent user settings (simple `key=value` text file).

use crate::params::{Params, PARAM_NAMES};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

/// Which effect processes the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Our own DSP (`dsp::Vinyl`) with its own presets.
    Builtin,
    /// A third-party Audio Unit (e.g. iZotope Vinyl) with its own parameters and presets.
    Plugin,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// Bundle-id prefixes of apps to process; empty = all system audio.
    pub targets: Vec<String>,
    pub mode: Mode,
    /// Selected Audio Unit effect; kept while in `Mode::Builtin` so switching back restores it.
    pub au: Option<String>,
    /// Run the built-in DSP after the AU.
    pub chain: bool,
    /// Last loaded/saved AU preset name.
    pub au_preset: Option<String>,
    /// The user's own built-in preset last loaded or saved; "Save" overwrites it.
    pub preset: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            targets: Vec::new(),
            mode: Mode::Builtin,
            au: None,
            chain: false,
            au_preset: None,
            preset: None,
        }
    }
}

pub fn support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Vinylizer")
}

/// Folder with the user's own presets for the built-in effect: `<name>.preset`, `key=value` lines.
pub fn user_preset_dir() -> PathBuf {
    support_dir().join("Presets")
}

pub fn user_preset_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.preset"))
}

/// User presets in `dir` as (name, values in `PARAM_NAMES` order), sorted by name.
pub fn user_presets(dir: &Path) -> Vec<(String, Vec<f32>)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("preset") {
            continue;
        }
        let (Some(name), Ok(text)) = (p.file_stem().and_then(|x| x.to_str()), std::fs::read_to_string(&p)) else {
            continue;
        };

        // Missing keys fall back to the default preset's values.
        let params = Params::new();
        apply_param_lines(&text, &params);
        out.push((name.to_string(), params.values()));
    }

    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

pub fn save_user_preset(dir: &Path, name: &str, params: &Params) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(user_preset_path(dir, name), param_lines(params))
}

fn param_lines(params: &Params) -> String {
    PARAM_NAMES
        .iter()
        .map(|n| format!("{n}={}\n", params.get(n).unwrap()))
        .collect()
}

fn apply_param_lines(text: &str, params: &Params) {
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if PARAM_NAMES.contains(&k) {
            if let Ok(x) = v.parse::<f32>() {
                let _ = params.set(k, x);
            }
        }
    }
}

/// File name for a plugin's saved state, derived from its display name.
pub fn au_state_file(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    support_dir().join(format!("{safe}.austate"))
}

impl Settings {
    /// The Audio Unit to host right now: only in plugin mode.
    pub fn active_au(&self) -> Option<&str> {
        match self.mode {
            Mode::Plugin => self.au.as_deref(),
            Mode::Builtin => None,
        }
    }

    pub fn parse(text: &str, params: &Params) -> Settings {
        let mut s = Settings::default();
        let mut mode = None;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "enabled" => s.enabled = v == "1",
                "idle_surface" => params.idle_surface.store(v == "1", Relaxed),
                "chain" => s.chain = v == "1",
                "mode" => {
                    mode = match v {
                        "plugin" => Some(Mode::Plugin),
                        "builtin" => Some(Mode::Builtin),
                        _ => None,
                    }
                }
                "au" => s.au = (!v.is_empty()).then(|| v.to_string()),
                "au_preset" => s.au_preset = (!v.is_empty()).then(|| v.to_string()),
                "preset" => s.preset = (!v.is_empty()).then(|| v.to_string()),
                "targets" => {
                    s.targets = v
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                        .collect()
                }
                _ if PARAM_NAMES.contains(&k) => {
                    if let Ok(x) = v.parse::<f32>() {
                        let _ = params.set(k, x);
                    }
                }
                _ => {}
            }
        }

        // Files written before modes existed: a saved plugin meant plugin mode.
        s.mode = mode.unwrap_or(if s.au.is_some() {
            Mode::Plugin
        } else {
            Mode::Builtin
        });
        s
    }

    pub fn serialize(&self, params: &Params) -> String {
        let mut out = format!(
            "enabled={}\nmode={}\nchain={}\nau={}\nau_preset={}\npreset={}\ntargets={}\n",
            self.enabled as u8,
            match self.mode {
                Mode::Builtin => "builtin",
                Mode::Plugin => "plugin",
            },
            self.chain as u8,
            self.au.as_deref().unwrap_or(""),
            self.au_preset.as_deref().unwrap_or(""),
            self.preset.as_deref().unwrap_or(""),
            self.targets.join(",")
        );
        out.push_str(&format!(
            "idle_surface={}\n",
            params.idle_surface.load(Relaxed) as u8
        ));
        out.push_str(&param_lines(params));
        out
    }

    pub fn load(path: &Path, params: &Params) -> Settings {
        std::fs::read_to_string(path)
            .map(|t| Self::parse(&t, params))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path, params: &Params) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.serialize(params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = Params::new();
        p.set("crackle", 0.42).unwrap();
        let s = Settings {
            enabled: false,
            targets: vec!["com.spotify.client".into(), "com.google.Chrome".into()],
            mode: Mode::Builtin,
            au: Some("iZotope: Vinyl".into()),
            chain: true,
            au_preset: Some("Jazz".into()),
            preset: Some("Пыльная 45".into()),
        };
        let text = s.serialize(&p);
        let p2 = Params::new();
        let s2 = Settings::parse(&text, &p2);
        assert_eq!(s, s2);
        assert!((p2.get("crackle").unwrap() - 0.42).abs() < 1e-6);
        assert_eq!(s2.active_au(), None, "plugin is remembered but not active");

        p.idle_surface.store(true, Relaxed);
        let p3 = Params::new();
        Settings::parse(&s.serialize(&p), &p3);
        assert!(p3.idle_surface.load(Relaxed));
    }

    #[test]
    fn user_presets_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vinylizer-presets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(user_presets(&dir).is_empty(), "missing folder is fine");

        let p = Params::new();
        p.set("crackle", 0.7).unwrap();
        p.set("rpm", 1.0).unwrap();
        save_user_preset(&dir, "Пыльная 45", &p).unwrap();
        std::fs::write(dir.join("notes.txt"), "crackle=1").unwrap();

        let list = user_presets(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "Пыльная 45");
        let q = Params::new();
        q.apply_values(&list[0].1);
        assert!(q.matches(&p.values()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_with_a_plugin_open_in_plugin_mode() {
        let p = Params::new();
        let s = Settings::parse("enabled=1\nau=iZotope: Vinyl\n", &p);
        assert_eq!(s.mode, Mode::Plugin);
        assert_eq!(s.active_au(), Some("iZotope: Vinyl"));
        let s = Settings::parse("enabled=1\n", &p);
        assert_eq!(s.mode, Mode::Builtin);
    }

    #[test]
    fn garbage_is_ignored() {
        let p = Params::new();
        let s = Settings::parse("junk\nwow=abc\nunknown=1\n", &p);
        assert_eq!(s, Settings::default());
    }
}

/// Append a timestamped line to the app log (dev builds log next to the sources).
pub fn log(msg: &str) {
    use std::io::Write;
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = if dev.is_dir() {
        dev.join("app.log")
    } else {
        support_dir().join("vinylizer.log")
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "[{t:.3}] {msg}");
    }
}
