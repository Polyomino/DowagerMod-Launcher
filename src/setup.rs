//! First-run setup: external tools the launcher shells out to (`git`
//! for version control, `gh` for bug reports) are installed on demand
//! via winget instead of failing.
//!
//! A fresh winget install is invisible to this process (our PATH is a
//! snapshot from launch), so after installing we resolve the new binary
//! at its well-known location and remember it via [`set_tool_path`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const GIT_WINGET_ID: &str = "Git.Git";
pub const GH_WINGET_ID: &str = "GitHub.cli";
/// winget installs can take minutes (download + installer + UAC wait).
const WINGET_TIMEOUT: Duration = Duration::from_secs(600);

static OVERRIDES: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

fn overrides() -> &'static Mutex<HashMap<String, PathBuf>> {
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remember where a freshly installed tool lives (our PATH predates
/// it). `None` clears back to plain PATH lookup.
pub fn set_tool_path(name: &str, path: Option<PathBuf>) {
    let mut map = overrides().lock().unwrap_or_else(|e| e.into_inner());
    match path {
        Some(p) => {
            map.insert(name.to_string(), p);
        }
        None => {
            map.remove(name);
        }
    }
}

/// Binary to spawn: the remembered install location, else the bare
/// name (normal PATH lookup).
pub fn tool_path(name: &str) -> PathBuf {
    overrides()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Demo/test hook: `DML_HIDE_<TOOL>=1` (e.g. `DML_HIDE_GIT=1`)
/// forces a tool missing so the install/auth flows can be tried on
/// machines that already have it. Inert unless explicitly set.
fn hidden_by_hook(tool: &str) -> bool {
    std::env::var(format!("DML_HIDE_{}", tool.to_uppercase())).as_deref() == Ok("1")
}

/// True when `<tool> --version` runs successfully (uses any remembered
/// location). Fast when the tool exists; fails fast when it doesn't.
pub fn tool_usable(name: &str) -> bool {
    if hidden_by_hook(name) {
        return false;
    }
    let mut cmd = std::process::Command::new(tool_path(name));
    crate::git::hide_child_console(&mut cmd);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when `gh` is authenticated (`gh auth status` exits 0).
pub fn gh_authed() -> bool {
    let mut cmd = std::process::Command::new(tool_path("gh"));
    crate::git::hide_child_console(&mut cmd);
    cmd.arg("auth")
        .arg("status")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn winget_available() -> bool {
    tool_usable("winget")
}

/// `winget install` arguments (pure constructor, for tests).
pub fn winget_install_args(package_id: &str) -> Vec<String> {
    [
        "install",
        "-e",
        "--id",
        package_id,
        "--silent",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Install a package via winget. The console is hidden, but a UAC
/// prompt from the underlying installer still surfaces — that comes
/// from the OS and needs the user's approval.
pub fn winget_install(package_id: &str) -> Result<(), String> {
    crate::log::append(&format!("winget install {package_id} started"));
    let mut cmd = std::process::Command::new("winget");
    crate::git::hide_child_console(&mut cmd);
    let mut child = cmd
        .args(winget_install_args(package_id))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run winget ({e})"))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = err.read_to_string(&mut stderr);
                }
                if status.success() {
                    crate::log::append(&format!("winget install {package_id} ok"));
                    return Ok(());
                }
                let detail = stderr
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("unknown winget error");
                crate::log::append(&format!("winget install {package_id} FAILED: {detail}"));
                return Err(format!("winget install failed: {detail}"));
            }
            Ok(None) => {
                if start.elapsed() > WINGET_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("winget install timed out after 10 minutes".to_string());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("failed waiting on winget: {e}")),
        }
    }
}

/// Install roots to probe for a binary winget just placed (our PATH
/// can't see it yet): both Program Files plus the per-user location.
fn program_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            out.push(PathBuf::from(dir));
        }
    }
    if let Some(local) = dirs::data_local_dir() {
        out.push(local.join("Programs"));
    }
    out
}

/// First `rel` under `roots` that exists as a file.
fn find_under(roots: &[PathBuf], rel: &Path) -> Option<PathBuf> {
    roots.iter().map(|r| r.join(rel)).find(|p| p.is_file())
}

/// Locate a freshly installed tool at its well-known path.
fn locate_fresh(tool: &str) -> Option<PathBuf> {
    let rel = match tool {
        "git" => Path::new("Git").join("cmd").join("git.exe"),
        "gh" => Path::new("GitHub CLI").join("gh.exe"),
        _ => return None,
    };
    find_under(&program_dirs(), &rel)
}

/// Ensure `tool` is usable, installing it via winget when needed.
/// Returns how to spawn it (bare name when PATH already resolves it).
/// Never touches the network when the tool already works.
pub fn ensure_tool(tool: &str, winget_id: &str) -> Result<PathBuf, String> {
    if tool_usable(tool) {
        set_tool_path(tool, None);
        return Ok(PathBuf::from(tool));
    }
    if !winget_available() {
        return Err(format!(
            "{tool} is not installed and winget is missing — install {tool} manually, then restart the launcher"
        ));
    }
    winget_install(winget_id)?;
    // The installer may have updated PATH for new processes only; our
    // snapshot predates it, so probe the well-known locations too.
    if tool_usable(tool) {
        set_tool_path(tool, None);
        return Ok(PathBuf::from(tool));
    }
    if let Some(found) = locate_fresh(tool) {
        set_tool_path(tool, Some(found.clone()));
        return Ok(found);
    }
    Err(format!(
        "{tool} installed but this session can't see it — restart the launcher"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_overrides_round_trip_on_unique_keys() {
        // Unique key: parallel tests never share it.
        let key = "dml-test-tool-path-registry";
        assert_eq!(tool_path(key), PathBuf::from(key));
        set_tool_path(key, Some(PathBuf::from("C:\\fake\\tool.exe")));
        assert_eq!(tool_path(key), PathBuf::from("C:\\fake\\tool.exe"));
        set_tool_path(key, None);
        assert_eq!(tool_path(key), PathBuf::from(key));
    }

    #[test]
    fn missing_tools_report_unusable() {
        assert!(!tool_usable("dml-no-such-tool-xyz"));
    }

    #[test]
    fn hide_hook_forces_tools_missing() {
        // No other test reads this variable, so set/remove is safe.
        std::env::set_var("DML_HIDE_GIT", "1");
        assert!(!tool_usable("git"));
        std::env::remove_var("DML_HIDE_GIT");
    }

    #[test]
    fn fresh_install_search_finds_nested_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let nested = root.join("Git").join("cmd");
        std::fs::create_dir_all(&nested).unwrap();
        let exe = nested.join("git.exe");
        std::fs::write(&exe, "fake").unwrap();
        let roots = std::slice::from_ref(&root);
        assert_eq!(
            find_under(
                roots,
                Path::new("Git").join("cmd").join("git.exe").as_path()
            ),
            Some(exe)
        );
        assert_eq!(
            find_under(roots, Path::new("Missing").join("tool.exe").as_path()),
            None
        );
    }

    #[test]
    fn winget_args_pin_exact_silent_install() {
        let expected = [
            "install",
            "-e",
            "--id",
            "Git.Git",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ];
        assert_eq!(
            winget_install_args("Git.Git"),
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }
}
