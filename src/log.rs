//! Append-only session log at `%LOCALAPPDATA%\DowagerMod-Launcher\launcher.log`.
//!
//! Every UI status line plus every git invocation lands here with a
//! timestamp, so a failed Deploy/Update can be diagnosed after the fact.
//! All entry points are infallible by design: logging must never break
//! the app (a missing home dir just means "no log").

use std::path::{Path, PathBuf};

const LOG_FILE_NAME: &str = "launcher.log";
const ROTATED_SUFFIX: &str = ".old";
/// Rotate when the log grows past this (checked once per session).
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Full path of the session log, if a local-app-data dir exists.
pub fn path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("DowagerMod-Launcher").join(LOG_FILE_NAME))
}

/// Session startup: rotate an oversized log, then write a header line.
pub fn start_session() {
    if cfg!(test) {
        return;
    }
    let Some(path) = path() else {
        return;
    };
    rotate_if_large(&path, MAX_LOG_BYTES);
    append_to(
        &path,
        &format!("=== launcher started v{} ===", env!("CARGO_PKG_VERSION")),
    );
}

/// Append one timestamped line. Silently drops when unwritable.
/// Unit tests never touch the real log (their git calls would pollute it).
pub fn append(line: &str) {
    if cfg!(test) {
        return;
    }
    let Some(path) = path() else {
        return;
    };
    append_to(&path, line);
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Append `line` to `path`, creating parent dirs. Best-effort: all
/// errors are swallowed (logging is never allowed to fail loudly).
fn append_to(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let text = format!("[{}] {line}\n", timestamp());
        let _ = file.write_all(text.as_bytes());
    }
}

/// If `path` exceeds `max_bytes`, move it aside to `<name>.old`
/// (deleting any previous `.old` first).
fn rotate_if_large(path: &Path, max_bytes: u64) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= max_bytes {
        return;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(ROTATED_SUFFIX);
    let rotated = PathBuf::from(rotated);
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, &rotated);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_is_launcher_log() {
        let path = path().expect("local app data dir exists on test machines");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(LOG_FILE_NAME)
        );
    }

    #[test]
    fn append_writes_timestamped_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join("test.log");
        append_to(&file, "hello");
        append_to(&file, "world");
        let body = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("hello"), "got: {}", lines[0]);
        assert!(lines[1].ends_with("world"), "got: {}", lines[1]);
        assert!(lines[0].starts_with('['), "got: {}", lines[0]);
    }

    #[test]
    fn rotation_kicks_in_only_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rot.log");
        std::fs::write(&file, "small").unwrap();
        rotate_if_large(&file, 1024);
        assert!(file.is_file(), "small file must stay put");

        std::fs::write(&file, "x".repeat(2048)).unwrap();
        rotate_if_large(&file, 1024);
        assert!(!file.is_file(), "large file must move aside");
        assert!(
            dir.path().join("rot.log.old").is_file(),
            "rotated file must exist"
        );
    }
}
