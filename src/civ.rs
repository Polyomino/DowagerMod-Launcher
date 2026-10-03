//! Finding and launching Civilization IV: Beyond the Sword with DowagerMod.
//!
//! The mod uses a wipe-and-restore install: repo files are overlaid onto the
//! live Steam install by `Install DowagerMod.bat`, so "launch with the mod"
//! means launching the live `Civ4BeyondSword.exe`.

use std::path::{Path, PathBuf};

/// Exe path inside the live install dir (see `CoreFiles/install.py`).
const EXE_RELATIVE: [&str; 2] = ["Beyond the Sword", "Civ4BeyondSword.exe"];
/// Sentinel written by the installer into the live install root.
const SENTINEL_NAME: &str = "_DOWAGERMOD_INSTALLED.txt";
/// Installer state written by `CoreFiles/install.py`.
const INSTALLER_CONFIG_RELATIVE: [&str; 2] = ["DowagerMod", "config.json"];

/// How the game will be launched (shown in the UI before launching).
#[derive(Debug, Clone)]
pub enum LaunchPlan {
    Exe(PathBuf),
    SteamUrl(String),
}

impl std::fmt::Display for LaunchPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exe(p) => write!(f, "{}", p.display()),
            Self::SteamUrl(u) => write!(f, "{u}"),
        }
    }
}

/// Read `%LOCALAPPDATA%\DowagerMod\config.json` written by the installer.
fn installer_config() -> Option<serde_json::Value> {
    let dir = dirs::data_local_dir()?;
    let path = INSTALLER_CONFIG_RELATIVE
        .iter()
        .fold(dir, |p, part| p.join(part));
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Live Civ4 install dir remembered by the installer, if any.
pub fn installed_game_dir() -> Option<PathBuf> {
    let dir = installer_config()?
        .get("install_dir")?
        .as_str()?
        .to_string();
    let p = PathBuf::from(dir);
    p.is_dir().then_some(p)
}

/// Mod version string recorded by the last installer run, if any.
pub fn last_deployed_version() -> Option<String> {
    installer_config()?
        .get("last_mod_version")?
        .as_str()
        .map(str::to_string)
}

/// Whether the live install currently carries the mod sentinel.
pub fn is_mod_deployed(game_dir: &Path) -> bool {
    game_dir.join(SENTINEL_NAME).exists()
}

fn exe_in(game_dir: &Path) -> PathBuf {
    EXE_RELATIVE
        .iter()
        .fold(game_dir.to_path_buf(), |p, part| p.join(part))
}

/// Candidate Steam library locations for the BTS folder.
pub fn candidate_game_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let folder = "Sid Meier's Civilization IV Beyond the Sword";
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(pf86) = std::env::var_os("ProgramFiles(x86)") {
        roots.push(
            PathBuf::from(pf86)
                .join("Steam")
                .join("steamapps")
                .join("common"),
        );
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        roots.push(
            PathBuf::from(pf)
                .join("Steam")
                .join("steamapps")
                .join("common"),
        );
    }
    for drive in ["C:", "D:", "E:", "F:"] {
        for base in [
            format!("{drive}\\Program Files (x86)\\Steam\\steamapps\\common"),
            format!("{drive}\\Program Files\\Steam\\steamapps\\common"),
            format!("{drive}\\Steam\\steamapps\\common"),
            format!("{drive}\\SteamLibrary\\steamapps\\common"),
        ] {
            roots.push(PathBuf::from(base));
        }
    }
    for root in roots {
        let dir = root.join(folder);
        if dir.is_dir() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// Resolve the launch plan: explicit override > installer config >
/// Steam library scan > `steam://run/<app_id>` fallback.
pub fn resolve(exe_override: &str, steam_app_id: u32) -> LaunchPlan {
    let trimmed = exe_override.trim();
    if !trimmed.is_empty() {
        return LaunchPlan::Exe(PathBuf::from(trimmed));
    }
    if let Some(dir) = installed_game_dir() {
        let exe = exe_in(&dir);
        if exe.is_file() {
            return LaunchPlan::Exe(exe);
        }
    }
    for dir in candidate_game_dirs() {
        let exe = exe_in(&dir);
        if exe.is_file() {
            return LaunchPlan::Exe(exe);
        }
    }
    LaunchPlan::SteamUrl(format!("steam://run/{steam_app_id}"))
}

/// Launch the game. Returns a human-readable description of what ran.
pub fn launch(plan: &LaunchPlan) -> Result<String, String> {
    match plan {
        LaunchPlan::Exe(exe) => {
            if !exe.is_file() {
                return Err(format!("game exe not found: {}", exe.display()));
            }
            let workdir = exe
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            std::process::Command::new(exe)
                .current_dir(workdir)
                .spawn()
                .map_err(|e| format!("failed to launch {}: {e}", exe.display()))?;
            Ok(format!("launched {}", exe.display()))
        }
        LaunchPlan::SteamUrl(url) => {
            open::that(url).map_err(|e| format!("failed to open {url}: {e}"))?;
            Ok(format!("opened {url}"))
        }
    }
}

/// Deploy the mod: run `Install DowagerMod.bat` (self-elevates via UAC),
/// falling back to the installer exe when the bat is missing.
pub fn deploy(repo: &std::path::Path) -> Result<String, String> {
    let bat = crate::git::deploy_script(repo);
    if bat.is_file() {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("\"{}\"", bat.display()))
            .current_dir(repo)
            .spawn()
            .map_err(|e| format!("failed to start installer: {e}"))?;
        return Ok("installer started (approve the UAC prompt)".to_string());
    }
    let exe = crate::git::installer_exe(repo);
    if exe.is_file() {
        std::process::Command::new(&exe)
            .current_dir(repo)
            .spawn()
            .map_err(|e| format!("failed to start installer: {e}"))?;
        return Ok("installer started (approve the UAC prompt)".to_string());
    }
    Err("no installer found: expected Install DowagerMod.bat in the repo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_url_contains_app_id() {
        let plan = resolve("C:\\no\\such\\dir\\x.exe", 8800);
        // Override is used verbatim even when it does not exist; existence
        // is checked at launch time so the UI can show the plan first.
        assert!(plan.to_string().contains("x.exe"));
        let fallback = LaunchPlan::SteamUrl("steam://run/8800".to_string());
        assert_eq!(fallback.to_string(), "steam://run/8800");
    }

    #[test]
    fn exe_path_joins_bts_relative_path() {
        let dir = PathBuf::from("C:\\game");
        assert_eq!(
            exe_in(&dir),
            PathBuf::from("C:\\game\\Beyond the Sword\\Civ4BeyondSword.exe")
        );
    }
}
