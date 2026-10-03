//! Self-update from GitHub Releases: on startup the launcher checks
//! `releases/latest`, offers the newer build, and — on accept —
//! downloads the exe, swaps it over the running one via a small batch
//! updater, and restarts.
//!
//! Windows cannot overwrite a running exe, hence the two-step swap: the
//! new binary lands next to the current one as `<name>.new.exe`, then a
//! hidden updater batch waits for our PID to exit, moves it over, and
//! starts it again.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// This repo (releases live here, unlike bug reports).
const UPDATE_REPO: &str = "Polyomino/DowagerMod-Launcher";

fn releases_latest_url() -> String {
    format!("https://api.github.com/repos/{UPDATE_REPO}/releases/latest")
}
/// Asset attached by `release.yml`.
const EXE_ASSET_NAME: &str = "dowager-mod-launcher.exe";
/// The release exe is ~10 MB; refuse anything absurd before writing it.
const MAX_ASSET_BYTES: u64 = 100 * 1024 * 1024;

/// A newer release worth offering.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub exe_url: String,
}

/// Whether this process should check for updates at all: never from a
/// debug build, and never for a binary living under a cargo `target/`
/// dir (a dev `cargo run --release` must not overwrite itself with a
/// published build).
pub fn should_check() -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    !exe_is_dev_build(&exe)
}

/// True when `exe` sits under a `target/` directory.
fn exe_is_dev_build(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "target")
}

/// Numeric dotted comparison of `local` (e.g. `0.1.0`) against `tag`
/// (e.g. `v0.2.0`, leading `v` tolerated). Unparsable versions compare
/// as not-newer: a weird tag must never trigger a swap.
pub fn is_newer(local: &str, tag: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.strip_prefix('v')
            .unwrap_or(s)
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    match (parse(local), parse(tag)) {
        (Some(mut l), Some(mut t)) => {
            let len = l.len().max(t.len());
            l.resize(len, 0);
            t.resize(len, 0);
            t > l
        }
        _ => false,
    }
}

/// Current launcher version (the exe being replaced).
pub fn local_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Web page for a release tag (manual-download fallback).
pub fn release_page_url(tag: &str) -> String {
    format!("https://github.com/{UPDATE_REPO}/releases/tag/{tag}")
}

/// Query `releases/latest` (short timeout: startup must not stall on a
/// dead network). Unauthenticated: fine for a public repo (60 req/hr).
pub fn fetch_latest() -> Result<ReleaseInfo, String> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let body = agent
        .get(&releases_latest_url())
        .set("User-Agent", "DowagerMod-Launcher")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => "no releases published yet".to_string(),
            ureq::Error::Status(code, _) => format!("GitHub API returned HTTP {code}"),
            ureq::Error::Transport(t) => format!("update check failed ({t})"),
        })?
        .into_string()
        .map_err(|e| format!("failed reading release info: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("bad release info: {e}"))?;
    parse_latest_release(&json).ok_or_else(|| "release has no launcher exe asset".to_string())
}

/// Pick the exe asset out of a `releases/latest` payload. Prereleases
/// are skipped even if the endpoint ever hands us one.
pub fn parse_latest_release(json: &serde_json::Value) -> Option<ReleaseInfo> {
    if json.get("prerelease").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let tag = json.get("tag_name")?.as_str()?.to_string();
    let exe_url = json
        .get("assets")?
        .as_array()?
        .iter()
        .filter(|a| a.get("name").and_then(|n| n.as_str()) == Some(EXE_ASSET_NAME))
        .filter_map(|a| a.get("browser_download_url")?.as_str())
        .next()?
        .to_string();
    Some(ReleaseInfo { tag, exe_url })
}

/// Download `url` to `dest`, refusing over-large bodies.
pub fn download(url: &str, dest: &Path) -> Result<(), String> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", "DowagerMod-Launcher")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let file =
        std::fs::File::create(dest).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    copy_capped(resp.into_reader(), file)
}

/// Stream `reader` into `file`, erroring when the body exceeds the cap
/// (a truncated exe must never pass as a successful download).
fn copy_capped<R: Read>(mut reader: R, mut file: std::fs::File) -> Result<(), String> {
    let mut limited = reader.by_ref().take(MAX_ASSET_BYTES);
    std::io::copy(&mut limited, &mut file).map_err(|e| format!("download failed: {e}"))?;
    let mut extra = [0u8; 1];
    if reader.read(&mut extra).unwrap_or(0) > 0 {
        return Err("release asset suspiciously large, refusing".to_string());
    }
    file.sync_all()
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(())
}

/// Where the download lands: next to the running exe.
pub fn pending_path(exe: &Path) -> PathBuf {
    let mut name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| EXE_ASSET_NAME.to_string());
    if let Some(dot) = name.rfind('.') {
        name.insert_str(dot, ".new");
    } else {
        name.push_str(".new");
    }
    exe.with_file_name(name)
}

/// Spawn the updater batch (hidden) and return: the caller must exit
/// immediately so the swap can proceed.
pub fn spawn_updater(exe: &Path, new_file: &Path) -> Result<(), String> {
    let pid = std::process::id();
    let bat = exe.with_file_name("dml-update.bat");
    std::fs::write(&bat, updater_script(pid, new_file, exe))
        .map_err(|e| format!("cannot write updater: {e}"))?;
    // Pass the path unquoted: Rust quotes it exactly once for cmd
    // (see the deploy quoting fix in `civ::deploy`).
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(&bat)
        .current_dir(exe.parent().unwrap_or_else(|| Path::new(".")))
        .spawn()
        .map_err(|e| format!("cannot start updater: {e}"))?;
    Ok(())
}

/// Updater batch: wait for our PID to vanish, move the new exe over
/// the old one, restart it, self-delete. The batch console is hidden
/// by the spawner; `move`/`start` paths are quoted for spaces.
pub fn updater_script(pid: u32, new_file: &Path, exe: &Path) -> String {
    format!(
        "@echo off\r\n\
         rem DowagerMod Launcher self-update: swap + restart, then self-delete.\r\n\
         :waitloop\r\n\
         tasklist /FI \"PID eq {pid}\" /NH 2>nul | find \"{pid}\" >nul\r\n\
         if %errorlevel%==0 (timeout /t 1 /nobreak >nul & goto waitloop)\r\n\
         move /Y \"{new}\" \"{exe}\" >nul\r\n\
         if errorlevel 1 (echo update failed: cannot replace the running exe & pause & del \"%~f0\" & exit /b 1)\r\n\
         start \"\" \"{exe}\"\r\n\
         del \"%~f0\"\r\n",
        new = new_file.display(),
        exe = exe.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_detection_compares_numerically() {
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.9.9", "v0.10.0"));
        assert!(!is_newer("0.2.0", "v0.2.0"));
        assert!(!is_newer("0.2.0", "v0.1.9"));
        assert!(!is_newer("0.1.0", "v0.1"));
        assert!(!is_newer("0.1", "v0.1.0"));
        assert!(!is_newer("0.1.0", "nightly"));
        assert!(!is_newer("dev", "v0.2.0"));
    }

    #[test]
    fn release_payload_picks_the_exe_asset() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "tag_name": "v0.2.0",
                "prerelease": false,
                "assets": [
                    {"name": "notes.txt", "browser_download_url": "https://x/notes.txt"},
                    {"name": "dowager-mod-launcher.exe",
                     "browser_download_url": "https://x/dml.exe"}
                ]
            }"#,
        )
        .unwrap();
        let rel = parse_latest_release(&json).expect("exe picked");
        assert_eq!(rel.tag, "v0.2.0");
        assert_eq!(rel.exe_url, "https://x/dml.exe");
    }

    #[test]
    fn release_payload_rejects_prerelease_and_missing_exe() {
        let pre: serde_json::Value =
            serde_json::from_str(r#"{"tag_name": "v9.0.0", "prerelease": true, "assets": []}"#)
                .unwrap();
        assert!(parse_latest_release(&pre).is_none());
        let no_exe: serde_json::Value = serde_json::from_str(
            r#"{"tag_name": "v0.2.0", "prerelease": false,
                "assets": [{"name": "other.zip", "browser_download_url": "https://x/o"}]}"#,
        )
        .unwrap();
        assert!(parse_latest_release(&no_exe).is_none());
        let bad: serde_json::Value = serde_json::from_str(r#"{"nope": true}"#).unwrap();
        assert!(parse_latest_release(&bad).is_none());
    }

    #[test]
    fn copy_capped_rejects_oversized_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.bin");
        copy_capped(
            std::io::Cursor::new(vec![7u8; 1024]),
            std::fs::File::create(&small).unwrap(),
        )
        .expect("small body passes");
        assert_eq!(std::fs::metadata(&small).unwrap().len(), 1024);

        // Just over the cap: all cap bytes plus one.
        let big = dir.path().join("big.bin");
        let body = vec![0u8; MAX_ASSET_BYTES as usize + 1];
        let err = copy_capped(
            std::io::Cursor::new(body),
            std::fs::File::create(&big).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("large"), "got: {err}");
    }

    #[test]
    fn pending_path_inserts_new_before_extension() {
        assert_eq!(
            pending_path(Path::new("C:\\app\\dowager-mod-launcher.exe")),
            PathBuf::from("C:\\app\\dowager-mod-launcher.new.exe")
        );
        assert_eq!(
            pending_path(Path::new("C:\\app\\tool")),
            PathBuf::from("C:\\app\\tool.new")
        );
    }

    #[test]
    fn updater_script_waits_swaps_restarts_and_cleans_up() {
        let script = updater_script(
            12345,
            Path::new("C:\\my app\\dml.new.exe"),
            Path::new("C:\\my app\\dml.exe"),
        );
        assert!(script.contains("PID eq 12345"), "got:\n{script}");
        assert!(
            script.contains("move /Y \"C:\\my app\\dml.new.exe\" \"C:\\my app\\dml.exe\""),
            "got:\n{script}"
        );
        assert!(
            script.contains("start \"\" \"C:\\my app\\dml.exe\""),
            "got:\n{script}"
        );
        assert!(script.contains("del \"%~f0\""), "got:\n{script}");
    }

    #[test]
    fn dev_builds_under_target_are_detected() {
        assert!(exe_is_dev_build(Path::new(
            "C:\\ws\\DowagerMod-Launcher\\target\\release\\dowager-mod-launcher.exe"
        )));
        assert!(exe_is_dev_build(Path::new("/home/u/proj/target/debug/dml")));
        assert!(!exe_is_dev_build(Path::new(
            "C:\\tools\\dml\\dowager-mod-launcher.exe"
        )));
    }
}
