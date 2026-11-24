//! Persistent user settings (simple `key=value` text file).

use crate::params::{Params, PARAM_NAMES};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// Bundle-id prefixes of apps to process; empty = all system audio.
    pub targets: Vec<String>,
    /// Audio Unit effect name; `None` = built-in DSP.
    pub au: Option<String>,
    /// Run the built-in DSP after the AU.
    pub chain: bool,
    /// Last loaded/saved AU preset name.
    pub au_preset: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            targets: Vec::new(),
            au: None,
            chain: false,
            au_preset: None,
        }
    }
}

pub fn support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Vinylizer")
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
    pub fn parse(text: &str, params: &Params) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "enabled" => s.enabled = v == "1",
                "chain" => s.chain = v == "1",
                "au" => s.au = (!v.is_empty()).then(|| v.to_string()),
                "au_preset" => s.au_preset = (!v.is_empty()).then(|| v.to_string()),
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
        s
    }

    pub fn serialize(&self, params: &Params) -> String {
        let mut out = format!(
            "enabled={}\nchain={}\nau={}\nau_preset={}\ntargets={}\n",
            self.enabled as u8,
            self.chain as u8,
            self.au.as_deref().unwrap_or(""),
            self.au_preset.as_deref().unwrap_or(""),
            self.targets.join(",")
        );
        for n in PARAM_NAMES {
            out.push_str(&format!("{n}={}\n", params.get(n).unwrap()));
        }
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
            au: Some("iZotope: Vinyl".into()),
            chain: true,
            au_preset: Some("Jazz".into()),
        };
        let text = s.serialize(&p);
        let p2 = Params::new();
        let s2 = Settings::parse(&text, &p2);
        assert_eq!(s, s2);
        assert!((p2.get("crackle").unwrap() - 0.42).abs() < 1e-6);
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
