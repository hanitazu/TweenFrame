//! Persistent settings in `%APPDATA%\TweenFrame\settings.ini` (plain `key=value`).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// 1 = off, 2 = 2x, 3 = 3x.
    pub multiplier: u32,
    /// Extra window classes to treat as a game window (comma separated in the file).
    pub extra_classes: Vec<String>,
    /// Optional window-title match (used for testing with other apps).
    pub title_contains: Option<String>,
    /// The one-time "I live in the tray now" notification was shown.
    pub tray_hint_shown: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { multiplier: 3, extra_classes: Vec::new(), title_contains: None, tray_hint_shown: false }
    }
}

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("TweenFrame")
}

fn path() -> PathBuf {
    app_dir().join("settings.ini")
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read_to_string(path()) {
            Ok(text) => Config::parse(&text),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(app_dir());
        let _ = std::fs::write(path(), self.to_text());
    }

    pub fn parse(text: &str) -> Self {
        let mut cfg = Config::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "multiplier" => {
                    if let Ok(m) = v.parse::<u32>() {
                        cfg.multiplier = m.clamp(1, 3);
                    }
                }
                "extra_classes" => {
                    cfg.extra_classes = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                }
                "title_contains" if !v.is_empty() => cfg.title_contains = Some(v.to_string()),
                "tray_hint_shown" => cfg.tray_hint_shown = v == "1",
                _ => {}
            }
        }
        cfg
    }

    pub fn to_text(&self) -> String {
        format!(
            "multiplier={}\nextra_classes={}\ntitle_contains={}\ntray_hint_shown={}\n",
            self.multiplier,
            self.extra_classes.join(","),
            self.title_contains.clone().unwrap_or_default(),
            if self.tray_hint_shown { 1 } else { 0 }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn roundtrip() {
        let cfg = Config {
            multiplier: 2,
            extra_classes: vec!["MyGame".into(), "Other".into()],
            title_contains: Some("Test".into()),
            tray_hint_shown: true,
        };
        let back = Config::parse(&cfg.to_text());
        assert_eq!(back.multiplier, 2);
        assert_eq!(back.extra_classes, vec!["MyGame".to_string(), "Other".to_string()]);
        assert_eq!(back.title_contains.as_deref(), Some("Test"));
        assert!(back.tray_hint_shown);
    }

    #[test]
    fn defaults_and_garbage() {
        let cfg = Config::parse("multiplier=9\nnonsense\nfoo=bar\ntitle_contains=\n");
        assert_eq!(cfg.multiplier, 3, "out-of-range multiplier is clamped");
        assert!(cfg.title_contains.is_none());
        assert!(!cfg.tray_hint_shown);
        assert_eq!(Config::parse("").multiplier, 3);
    }
}
