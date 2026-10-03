//! Finding and launching Civilization IV: Beyond the Sword with DowagerMod.
//!
//! The mod uses a wipe-and-restore install: repo files are overlaid onto the
//! live Steam install by `Install DowagerMod.bat`, so "launch with the mod"
//! means launching the live `Civ4BeyondSword.exe`.

use std::path::{Path, PathBuf};
use std::process::Child;

/// Exe path inside the live install dir (see `CoreFiles/install.py`).
const EXE_RELATIVE: [&str; 2] = ["Beyond the Sword", "Civ4BeyondSword.exe"];
/// Sentinel written by the installer into the live install root.
const SENTINEL_NAME: &str = "_DOWAGERMOD_INSTALLED.txt";
/// Installer state written by `CoreFiles/install.py`.
const INSTALLER_CONFIG_RELATIVE: [&str; 2] = ["DowagerMod", "config.json"];
/// Image name of the running game process (for the deploy guard).
const GAME_EXE_NAME: &str = "Civ4BeyondSword.exe";
const GAME_EXE_NAME_LOWER: &str = "civ4beyondsword.exe";
/// Cross-process deploy mutex: held open for the whole install.
const DEPLOY_LOCK_NAME: &str = "DowagerMod-Launcher.deploy.lock";

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
    serde_json::from_str(&installer_config_text()?).ok()
}

/// Raw installer config text, for bug reports (the caller redacts it).
pub fn installer_config_text() -> Option<String> {
    let dir = dirs::data_local_dir()?;
    let path = INSTALLER_CONFIG_RELATIVE
        .iter()
        .fold(dir, |p, part| p.join(part));
    std::fs::read_to_string(path).ok()
}

/// Civ4 Beyond the Sword log folder
/// (`Documents\My Games\Beyond the Sword\Logs`).
pub fn civ_log_dir() -> Option<PathBuf> {
    dirs::document_dir().map(|d| d.join("My Games").join("Beyond the Sword").join("Logs"))
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

/// ISO timestamp recorded by the last installer run, if any.
pub fn last_deployed_at() -> Option<String> {
    installer_config()?
        .get("last_install_at")?
        .as_str()
        .map(str::to_string)
}

/// Whether the live install matches the current checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMatch {
    /// No deployed version recorded (or unreadable on either side).
    Unknown,
    /// Deployed version string equals the checkout's `git describe`.
    Matches,
    /// Anything else: update/switch happened since, or tags moved.
    Differs,
}

/// Compare the installer's recorded version against the checkout's
/// current `git describe` (the same command `install.py` uses).
pub fn live_match(repo: &Path) -> LiveMatch {
    match_versions(
        last_deployed_version().as_deref(),
        &crate::git::describe(repo),
    )
}

/// Pure comparison behind [`live_match`], for unit tests.
pub fn match_versions(deployed: Option<&str>, current: &str) -> LiveMatch {
    let Some(deployed) = deployed else {
        return LiveMatch::Unknown;
    };
    if deployed.is_empty() || deployed == "unknown" || current.is_empty() || current == "unknown" {
        return LiveMatch::Unknown;
    }
    if deployed == current {
        LiveMatch::Matches
    } else {
        LiveMatch::Differs
    }
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

/// True when the game is currently running (checked via `tasklist`).
/// Deploying over a running game corrupts the install (locked files
/// under a wipe-and-restore), so deploy refuses in that case.
pub fn is_game_running() -> bool {
    let mut cmd = std::process::Command::new("tasklist");
    // `tasklist` is a console program: hide it like the git calls.
    crate::git::hide_child_console(&mut cmd);
    match cmd
        .arg("/FI")
        .arg(format!("IMAGENAME eq {GAME_EXE_NAME}"))
        .arg("/FO")
        .arg("CSV")
        .arg("/NH")
        .output()
    {
        Ok(o) if o.status.success() => tasklist_shows_exe(&String::from_utf8_lossy(&o.stdout)),
        // `tasklist` missing/failed: fail open (can't prove it runs).
        _ => false,
    }
}

/// Parse `tasklist /FO CSV /NH` output: a match row quotes the exe name,
/// a miss prints an INFO line without it.
fn tasklist_shows_exe(output: &str) -> bool {
    output.to_lowercase().contains(GAME_EXE_NAME_LOWER)
}

#[cfg(windows)]
fn exclusive_open(opts: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    // share_mode(0): no other opener — not even this process — while held.
    // The OS releases it on crash, so the lock can never go stale.
    opts.share_mode(0);
}

#[cfg(not(windows))]
fn exclusive_open(_opts: &mut std::fs::OpenOptions) {}

/// Cross-process deploy lock, held open for the install duration.
/// Dropping it releases. A failed acquire means another launcher window
/// is mid-deploy (a crash can't wedge it: the OS frees the handle).
pub(crate) struct DeployLock {
    _held: std::fs::File,
}

/// Try to take the deploy lock. At most one holder machine-wide.
pub(crate) fn acquire_deploy_lock() -> Option<DeployLock> {
    let path = std::env::temp_dir().join(DEPLOY_LOCK_NAME);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    exclusive_open(&mut opts);
    opts.open(path).ok().map(|f| DeployLock { _held: f })
}

/// Deploy the mod: run `Install DowagerMod.bat` (self-elevates via UAC),
/// falling back to the installer exe when the bat is missing. Refuses
/// while the game runs. Returns the child process so the caller can wait
/// on it (single-flight: no second deploy until this one ends).
///
/// Note: the child's console is intentionally NOT hidden — the installer
/// interacts with the user in its own window.
pub fn deploy(repo: &Path) -> Result<Child, String> {
    let bat = crate::git::deploy_script(repo);
    let use_bat = bat.is_file();
    let exe = crate::git::installer_exe(repo);
    if !use_bat && !exe.is_file() {
        return Err("no installer found: expected Install DowagerMod.bat in the repo".to_string());
    }
    if is_game_running() {
        return Err("deploy refused: Civ4 is running — quit the game first".to_string());
    }
    if use_bat {
        // Pass the path unquoted: Rust quotes it exactly once for cmd.
        // (Pre-quoting here gets escaped a second time, and cmd then
        // fails to find the bat at all.)
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(&bat)
            .current_dir(repo)
            .spawn()
            .map_err(|e| format!("failed to start installer: {e}"))
    } else {
        std::process::Command::new(&exe)
            .current_dir(repo)
            .spawn()
            .map_err(|e| format!("failed to start installer: {e}"))
    }
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

    /// Deploy must survive spaces: the bat name itself
    /// (`Install DowagerMod.bat`) has them, so quoting has to be right
    /// even for space-free repo dirs.
    #[cfg(windows)]
    #[test]
    fn deploy_runs_bat_with_spaces_in_path() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("dir with spaces");
        std::fs::create_dir(&repo).unwrap();
        let marker = repo.join("deployed.marker");
        std::fs::write(
            repo.join("Install DowagerMod.bat"),
            format!("@echo off\r\necho ok > \"{}\"\r\n", marker.display()),
        )
        .unwrap();
        let mut child = deploy(&repo).expect("deploy spawns");
        let mut waited = 0;
        while !marker.is_file() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            marker.is_file(),
            "bat with spaces in path never ran (quoting bug?)"
        );
    }

    #[test]
    fn tasklist_parse_spots_running_game() {
        assert_eq!(GAME_EXE_NAME.to_lowercase(), GAME_EXE_NAME_LOWER);
        assert!(tasklist_shows_exe(
            "\"Civ4BeyondSword.exe\",\"1234\",\"Console\",\"1\",\"1,024 K\""
        ));
        assert!(tasklist_shows_exe(
            "\"civ4beyondsword.EXE\",\"1\",\"C\",\"1\",\"1 K\""
        ));
        assert!(!tasklist_shows_exe(
            "INFO: No tasks are running which match the specified criteria."
        ));
        assert!(!tasklist_shows_exe(""));
    }

    #[test]
    fn deploy_lock_single_flight() {
        let held = acquire_deploy_lock().expect("first acquire succeeds");
        #[cfg(windows)]
        {
            assert!(
                acquire_deploy_lock().is_none(),
                "second acquire refused while held"
            );
        }
        drop(held);
        assert!(
            acquire_deploy_lock().is_some(),
            "re-acquire succeeds after release"
        );
    }

    #[test]
    fn report_paths_stay_well_formed() {
        if let Some(dir) = civ_log_dir() {
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("Logs"));
        }
        // Read-only probe: present on dev machines, absent on CI.
        if let Some(text) = installer_config_text() {
            assert!(
                serde_json::from_str::<serde_json::Value>(&text).is_ok(),
                "installer config must stay valid JSON"
            );
        }
    }

    #[test]
    fn version_matching_handles_unknowns() {
        assert_eq!(match_versions(Some("abc123"), "abc123"), LiveMatch::Matches);
        assert_eq!(match_versions(Some("abc123"), "def456"), LiveMatch::Differs);
        assert_eq!(match_versions(None, "abc123"), LiveMatch::Unknown);
        assert_eq!(
            match_versions(Some("unknown"), "abc123"),
            LiveMatch::Unknown
        );
        assert_eq!(
            match_versions(Some("abc123"), "unknown"),
            LiveMatch::Unknown
        );
        assert_eq!(match_versions(Some(""), "abc123"), LiveMatch::Unknown);
    }
}
