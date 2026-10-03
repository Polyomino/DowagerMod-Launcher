//! Launcher configuration: where the DowagerMod repo lives, how Civ4 is launched.
//!
//! Persisted to `%APPDATA%\DowagerMod-Launcher\config.json` so the mod path
//! stays configurable per machine.

use std::path::PathBuf;

/// Steam App ID for "Sid Meier's Civilization IV: Beyond the Sword".
/// Verified against the Steam store (`https://store.steampowered.com/app/8800/`).
pub const DEFAULT_STEAM_APP_ID: u32 = 8800;

/// Folder name of the DowagerMod checkout (repo living next to the launcher).
pub const DEFAULT_REPO_DIR_NAME: &str = "DowagerMod";

/// Parse the Steam app id settings box: garbage/empty keeps the previous
/// value instead of silently resetting to the default.
pub fn parse_app_id(text: &str, fallback: u32) -> u32 {
    text.trim().parse().unwrap_or(fallback)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LauncherConfig {
    /// Full path to the DowagerMod git checkout. Empty = auto-detect.
    #[serde(default)]
    pub mod_repo_path: String,
    /// Steam App ID used for the `steam://run/<id>` launch fallback.
    #[serde(default = "default_app_id")]
    pub steam_app_id: u32,
    /// Optional full path to `Civ4BeyondSword.exe`. Empty = auto-detect.
    #[serde(default)]
    pub civ_exe_override: String,
}

fn default_app_id() -> u32 {
    DEFAULT_STEAM_APP_ID
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            mod_repo_path: String::new(),
            steam_app_id: DEFAULT_STEAM_APP_ID,
            civ_exe_override: String::new(),
        }
    }
}

impl LauncherConfig {
    pub fn file_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("DowagerMod-Launcher").join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::file_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(path, text)
    }

    /// Resolve the mod repo directory: configured path first, then a
    /// `DowagerMod` folder next to the launcher executable / working dir.
    pub fn repo_path(&self) -> Option<PathBuf> {
        if !self.mod_repo_path.trim().is_empty() {
            let p = PathBuf::from(self.mod_repo_path.trim());
            if p.is_dir() {
                return Some(p);
            }
            return None;
        }
        default_repo_path()
    }
}

/// Auto-detect `<...>/DowagerMod` next to the running executable or the
/// current working directory (covers `cargo run` from the launcher project).
pub fn default_repo_path() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(DEFAULT_REPO_DIR_NAME));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join(DEFAULT_REPO_DIR_NAME));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(DEFAULT_REPO_DIR_NAME));
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join(DEFAULT_REPO_DIR_NAME));
        }
    }
    candidates.into_iter().find(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_bts_app_id() {
        assert_eq!(LauncherConfig::default().steam_app_id, 8800);
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = LauncherConfig {
            mod_repo_path: "C:\\mods\\DowagerMod".to_string(),
            steam_app_id: 8800,
            civ_exe_override: String::new(),
        };
        let text = serde_json::to_string(&cfg).unwrap();
        let back: LauncherConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.mod_repo_path, cfg.mod_repo_path);
        assert_eq!(back.steam_app_id, 8800);
    }

    #[test]
    fn explicit_repo_path_is_used_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LauncherConfig {
            mod_repo_path: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.repo_path(), Some(dir.path().to_path_buf()));
    }

    #[test]
    fn bad_app_id_keeps_previous_value() {
        assert_eq!(parse_app_id("1234", 8800), 1234);
        assert_eq!(parse_app_id("", 8800), 8800);
        assert_eq!(parse_app_id("xyz", 8800), 8800);
    }

    #[test]
    fn missing_explicit_repo_path_resolves_to_none() {
        let cfg = LauncherConfig {
            mod_repo_path: "C:\\definitely\\not\\a\\real\\path\\xyz".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.repo_path(), None);
    }
}
