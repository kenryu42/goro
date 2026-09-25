//! User settings: a JSON file in the platform config dir (`GORO_CONFIG_DIR` overrides it).
//! Unknown keys are ignored and missing keys take defaults, so the file only needs what
//! the user changes; an invalid file keeps the defaults and reports why.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeSetting,
    /// Monospace font; `null` uses the platform default.
    pub font_family: Option<String>,
    pub font_size: f32,
    pub diff_layout: DiffLayout,
    /// Shows Goro from anywhere while it's running (e.g. `"cmd+alt+g"`); `null` disables.
    pub global_hotkey: Option<String>,
    /// Extra bindings for the diff: keystroke (`"ctrl-j"`) → action (`"goro::NextHunk"`).
    /// An empty action removes the key's default binding.
    pub keybindings: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeSetting::System,
            font_family: None,
            font_size: 12.5,
            diff_layout: DiffLayout::Unified,
            global_hotkey: Some(
                if cfg!(target_os = "macos") {
                    "cmd+alt+g"
                } else {
                    "ctrl+alt+g"
                }
                .to_string(),
            ),
            keybindings: BTreeMap::new(),
        }
    }
}

/// `GORO_CONFIG_DIR`, else the platform config dir, joined with `settings.json`.
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("GORO_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("Goro")))
        .map(|d| d.join("settings.json"))
}

/// Load settings from `path`. A missing file is the defaults; an invalid one is the
/// defaults plus an error message for the user.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (Settings::default(), None);
        }
        Err(err) => {
            return (
                Settings::default(),
                Some(format!("{}: {err}", path.display())),
            );
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(settings) if !(6.0..=48.0).contains(&settings.font_size) => (
            Settings {
                font_size: Settings::default().font_size,
                ..settings
            },
            Some(format!(
                "{}: font_size must be between 6 and 48",
                path.display()
            )),
        ),
        Ok(settings) => (settings, None),
        Err(err) => (
            Settings::default(),
            Some(format!(
                "{} is invalid ({err}); using defaults",
                path.display()
            )),
        ),
    }
}

/// Write the defaults (with every key, as documentation) if there's no file yet.
pub fn write_default_if_missing(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&Settings::default()).expect("settings serialize");
    text.push('\n');
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("settings.json")),
            (Settings::default(), None)
        );
    }

    #[test]
    fn partial_files_keep_defaults_for_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{ "theme": "dark", "keybindings": { "ctrl-j": "goro::NextHunk" }, "future_key": 1 }"#,
        )
        .unwrap();
        let (settings, error) = load(&path);
        assert_eq!(error, None);
        assert_eq!(settings.theme, ThemeSetting::Dark);
        assert_eq!(settings.font_size, 12.5);
        assert_eq!(settings.keybindings["ctrl-j"], "goro::NextHunk");
    }

    #[test]
    fn invalid_files_report_and_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "theme": "sepia" }"#).unwrap();
        let (settings, error) = load(&path);
        assert_eq!(settings, Settings::default());
        assert!(error.unwrap().contains("invalid"));

        std::fs::write(&path, r#"{ "font_size": 200 }"#).unwrap();
        let (settings, error) = load(&path);
        assert_eq!(settings.font_size, 12.5);
        assert!(error.unwrap().contains("font_size"));
    }

    #[test]
    fn default_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        write_default_if_missing(&path).unwrap();
        assert_eq!(load(&path), (Settings::default(), None));
        std::fs::write(&path, r#"{ "theme": "light" }"#).unwrap();
        write_default_if_missing(&path).unwrap();
        assert_eq!(
            load(&path).0.theme,
            ThemeSetting::Light,
            "never overwritten"
        );
    }
}
