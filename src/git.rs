//! Git operations for the DowagerMod checkout.
//!
//! Everything shells out to the `git` CLI (already required by the mod
//! workflow) so the launcher stays dependency-light and starts fast.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// One local branch, newest-commit-first as produced by
/// `git for-each-ref --sort=-committerdate`.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub commit_time_unix: i64,
    pub short_hash: String,
    pub subject: String,
    pub is_current: bool,
    /// True when the branch exists only on the remote (checking it out
    /// creates a local tracking branch).
    pub remote_only: bool,
}

/// Snapshot of the checkout shown in the header card.
#[derive(Debug, Clone, Default)]
pub struct RepoStatus {
    pub branch: String,
    pub detached: bool,
    pub short_hash: String,
    pub subject: String,
    pub commit_date: String,
    /// Commits the checkout is behind the branch tip.
    pub behind: u64,
    /// Commits the checkout is ahead of the branch tip (unpushed).
    pub ahead: u64,
    /// What the tip was compared against, e.g. `origin/main` or `main`.
    pub tip_ref: String,
    pub fetched_at: Option<SystemTime>,
    pub fetch_error: Option<String>,
}

/// How long any single git invocation may run before it is killed.
/// Local-only commands finish in milliseconds; this only bites when the
/// network hangs (no route to the remote, VPN blackhole) so the UI can
/// never wedge on `busy` forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(90);

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        // Never block on an interactive credential prompt: fail fast
        // instead so the UI can show the error.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run git: {e}"))?;
    // Drain both pipes on helper threads so a chatty child can never
    // wedge on a full buffer while we poll for its exit.
    let mut stdout = child.stdout.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout.as_mut() {
            let _ = out.read_to_end(&mut buf);
        }
        buf
    });
    let mut stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(err) = stderr.as_mut() {
            let _ = err.read_to_end(&mut buf);
        }
        buf
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = out_thread.join().unwrap_or_default();
                let err = err_thread.join().unwrap_or_default();
                if status.success() {
                    return Ok(String::from_utf8_lossy(&out).trim().to_string());
                }
                return Err(String::from_utf8_lossy(&err).trim().to_string());
            }
            Ok(None) => {
                if start.elapsed() > GIT_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    let what = args.first().unwrap_or(&"?");
                    return Err(format!(
                        "git {what} timed out after {}s",
                        GIT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed to wait on git: {e}")),
        }
    }
}

/// `git fetch --prune` on the default remote (the current branch's
/// upstream remote, else `origin`). Failures (e.g. offline) are returned,
/// never raised: the UI keeps the last-known status and shows the error.
pub fn fetch(repo: &Path) -> Result<(), String> {
    git(repo, &["fetch", "--prune", "--quiet"]).map(|_| ())
}

/// Current branch name, or `None` when HEAD is detached.
pub fn current_branch(repo: &Path) -> Option<String> {
    match git(repo, &["branch", "--show-current"]) {
        Ok(name) if !name.is_empty() => Some(name),
        _ => None,
    }
}

/// Full status snapshot. `fetch_first` controls whether a network fetch
/// runs before measuring (background refresh passes true; the very first
/// paint passes false so the window appears instantly).
pub fn status(repo: &Path, fetch_first: bool) -> Result<RepoStatus, String> {
    if !repo.join(".git").exists() && !is_git_dir(repo) {
        return Err(format!("not a git checkout: {}", repo.to_string_lossy()));
    }
    let mut out = RepoStatus::default();

    if fetch_first {
        match fetch(repo) {
            Ok(()) => out.fetched_at = Some(SystemTime::now()),
            Err(e) => out.fetch_error = Some(e),
        }
    }

    out.short_hash = git(repo, &["rev-parse", "--short", "HEAD"])?;
    out.subject = git(repo, &["log", "-1", "--format=%s"]).unwrap_or_default();
    out.commit_date = git(repo, &["log", "-1", "--format=%cs"]).unwrap_or_default();

    match current_branch(repo) {
        Some(branch) => {
            out.branch = branch.clone();
            out.tip_ref = tip_ref(repo, &branch);
            out.behind = count(repo, &format!("HEAD..{}", out.tip_ref)).unwrap_or(0);
            out.ahead = count(repo, &format!("{}..HEAD", out.tip_ref)).unwrap_or(0);
        }
        None => {
            out.detached = true;
            out.branch = format!("detached @ {}", out.short_hash);
        }
    }
    Ok(out)
}

/// Tip ref the checkout is compared against (and updated to): the
/// configured upstream (`@{u}`) when there is one, else `origin/<branch>`,
/// else the local branch itself (no remote tip known).
fn tip_ref(repo: &Path, branch: &str) -> String {
    if let Ok(upstream) = git(repo, &["rev-parse", "--abbrev-ref", "@{u}"]) {
        if !upstream.is_empty() {
            return upstream;
        }
    }
    let origin = format!("origin/{branch}");
    if git(repo, &["rev-parse", "--verify", "--quiet", &origin]).is_ok() {
        return origin;
    }
    branch.to_string()
}

fn is_git_dir(repo: &Path) -> bool {
    git(repo, &["rev-parse", "--git-dir"]).is_ok()
}

fn count(repo: &Path, range: &str) -> Result<u64, String> {
    let text = git(repo, &["rev-list", "--count", range])?;
    text.parse::<u64>()
        .map_err(|e| format!("bad rev-list count {text:?}: {e}"))
}

/// Local plus remote branches, globally ordered by latest commit time
/// (newest first). A remote branch shadowed by a same-named local branch
/// is dropped.
pub fn branches(repo: &Path) -> Result<Vec<BranchInfo>, String> {
    let current = current_branch(repo).unwrap_or_default();
    let text = git(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)|%(committerdate:unix)|%(objectname:short)|%(subject)",
            "refs/heads/",
            "refs/remotes/",
        ],
    )?;
    Ok(dedupe_branches(
        text.lines()
            .filter_map(|l| parse_branch_line(l, &current))
            .collect(),
    ))
}

/// Drop remote rows shadowed by a same-named local branch, plus
/// duplicate remote-only names (same branch on several remotes), keeping
/// the global newest-first order.
pub fn dedupe_branches(list: Vec<BranchInfo>) -> Vec<BranchInfo> {
    use std::collections::HashSet;
    let local: HashSet<String> = list
        .iter()
        .filter(|b| !b.remote_only)
        .map(|b| b.name.clone())
        .collect();
    let mut seen_remote: HashSet<String> = HashSet::new();
    list.into_iter()
        .filter(|b| {
            if b.remote_only {
                !local.contains(&b.name) && seen_remote.insert(b.name.clone())
            } else {
                true
            }
        })
        .collect()
}

/// Parse one `for-each-ref` line. `splitn(4, ...)` keeps `|` inside a
/// subject intact. (A `|` inside the branch name itself — legal but
/// vanishingly rare — fails to parse and the row is skipped.)
pub fn parse_branch_line(line: &str, current: &str) -> Option<BranchInfo> {
    let mut parts = line.splitn(4, '|');
    let refname = parts.next()?.trim();
    let time = parts.next()?.trim().parse::<i64>().ok()?;
    let hash = parts.next()?.trim().to_string();
    let subject = parts.next().unwrap_or("").trim().to_string();
    let (name, remote_only) = strip_refname(refname)?;
    if name.is_empty() {
        return None;
    }
    Some(BranchInfo {
        is_current: !remote_only && name == current,
        name: name.to_string(),
        commit_time_unix: time,
        short_hash: hash,
        subject,
        remote_only,
    })
}

/// Split `refs/heads/foo` / `refs/remotes/origin/foo` into display name
/// plus remote flag. `None` for HEAD symrefs and anything unexpected.
fn strip_refname(refname: &str) -> Option<(&str, bool)> {
    if let Some(name) = refname.strip_prefix("refs/heads/") {
        if name.is_empty() || name == "HEAD" {
            return None;
        }
        Some((name, false))
    } else if let Some(rest) = refname.strip_prefix("refs/remotes/") {
        let (_remote, name) = rest.split_once('/')?;
        if name.is_empty() || name == "HEAD" {
            return None;
        }
        Some((name, true))
    } else {
        None
    }
}

/// Case-insensitive substring filter over branch name + subject.
pub fn filter_branches<'a>(branches: &'a [BranchInfo], query: &str) -> Vec<&'a BranchInfo> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return branches.iter().collect();
    }
    branches
        .iter()
        .filter(|b| b.name.to_lowercase().contains(&q) || b.subject.to_lowercase().contains(&q))
        .collect()
}

/// Fast-forward the current branch to its remote tip (the same ref
/// `status` compares against). Works without an upstream configuration;
/// refuses on a detached HEAD with a clear message instead of a cryptic
/// git failure.
pub fn update_to_latest(repo: &Path) -> Result<String, String> {
    let branch = current_branch(repo)
        .ok_or_else(|| "not on a branch — check out a branch to update".to_string())?;
    let tip = tip_ref(repo, &branch);
    if tip == branch {
        return Err(format!("branch {branch} has no remote tip to update to"));
    }
    git(repo, &["fetch", "--prune", "--quiet"])?;
    git(repo, &["merge", "--ff-only", &tip])
}

/// Switch branches. A remote-only name creates a local tracking branch via
/// git's DWIM (`git checkout <name>`). Fails with git's message when the
/// tree is dirty.
pub fn checkout(repo: &Path, branch: &str) -> Result<String, String> {
    // Argv (no shell) makes injection impossible, but a name starting with
    // `-` would still parse as a flag (`checkout -f` discards local
    // changes!), and `..` / whitespace can never be a real branch name.
    if branch.trim().is_empty()
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.chars().any(char::is_whitespace)
    {
        return Err(format!("refusing to check out odd branch name: {branch:?}"));
    }
    git(repo, &["checkout", branch])
}

/// Short human age, e.g. `3d ago`. Pure function for unit tests.
pub fn relative_time(commit_time_unix: i64, now_unix: i64) -> String {
    let delta = (now_unix - commit_time_unix).max(0);
    const MIN: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    if delta < MIN {
        "just now".to_string()
    } else if delta < HOUR {
        format!("{}m ago", delta / MIN)
    } else if delta < DAY {
        format!("{}h ago", delta / HOUR)
    } else if delta < 30 * DAY {
        format!("{}d ago", delta / DAY)
    } else if delta < 365 * DAY {
        format!("{}mo ago", delta / (30 * DAY))
    } else {
        format!("{}y ago", delta / (365 * DAY))
    }
}

/// Header status line, e.g. `3 behind origin/main` / `up to date`.
pub fn distance_text(status: &RepoStatus) -> String {
    if status.detached {
        return "detached HEAD — check out a branch to update".to_string();
    }
    match (status.behind, status.ahead) {
        (0, 0) => {
            if status.tip_ref == status.branch {
                format!("up to date with {} (no remote tip found)", status.tip_ref)
            } else {
                format!("up to date with {}", status.tip_ref)
            }
        }
        (b, 0) => format!("{b} commit{} behind {}", plural(b), status.tip_ref),
        (0, a) => format!("{a} commit{} ahead of {}", plural(a), status.tip_ref),
        (b, a) => format!(
            "{} commit{} behind / {} commit{} ahead of {}",
            b,
            plural(b),
            a,
            plural(a),
            status.tip_ref
        ),
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Absolute path of the friend-facing deploy script in the checkout.
pub fn deploy_script(repo: &Path) -> PathBuf {
    repo.join("Install DowagerMod.bat")
}

/// Absolute path of the built installer exe in the checkout.
pub fn installer_exe(repo: &Path) -> PathBuf {
    repo.join("CoreFiles")
        .join("dist")
        .join("DowagerMod-Installer")
        .join("DowagerMod-Installer.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, subject: &str) -> BranchInfo {
        BranchInfo {
            name: name.to_string(),
            commit_time_unix: 0,
            short_hash: "abc1234".to_string(),
            subject: subject.to_string(),
            is_current: false,
            remote_only: false,
        }
    }

    fn remote(name: &str) -> BranchInfo {
        BranchInfo {
            remote_only: true,
            ..branch(name, "")
        }
    }

    #[test]
    fn parses_for_each_ref_line() {
        let b =
            parse_branch_line("refs/heads/main|1750000000|abc1234|Fix chatter", "main").unwrap();
        assert_eq!(b.name, "main");
        assert_eq!(b.commit_time_unix, 1_750_000_000);
        assert_eq!(b.short_hash, "abc1234");
        assert_eq!(b.subject, "Fix chatter");
        assert!(b.is_current);
        assert!(!b.remote_only);
    }

    #[test]
    fn parses_remote_branch_line() {
        let b = parse_branch_line(
            "refs/remotes/origin/leaders|1750000000|abc1234|Leader work",
            "main",
        )
        .unwrap();
        assert_eq!(b.name, "leaders");
        assert!(b.remote_only);
        assert!(!b.is_current);
    }

    #[test]
    fn subject_may_contain_pipes() {
        let b = parse_branch_line("refs/heads/x|1|h|a|b|c", "").unwrap();
        assert_eq!(b.subject, "a|b|c");
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse_branch_line("", "").is_none());
        assert!(parse_branch_line("only-name", "").is_none());
        assert!(parse_branch_line("x|not-a-time|h|s", "").is_none());
        assert!(parse_branch_line("|1|h|s", "").is_none());
        assert!(parse_branch_line("refs/heads/|1|h|s", "").is_none());
        assert!(parse_branch_line("refs/remotes/origin|1|h|s", "").is_none());
    }

    #[test]
    fn skips_head_symrefs() {
        assert!(parse_branch_line("refs/remotes/origin/HEAD|1|h|s", "").is_none());
        assert!(parse_branch_line("refs/heads/HEAD|1|h|s", "").is_none());
    }

    #[test]
    fn dedupe_drops_shadowed_remotes_keeping_order() {
        let list = vec![
            remote("zebra"),
            branch("main", ""),
            remote("main"),
            remote("leaders"),
        ];
        let out = dedupe_branches(list);
        let names: Vec<(&str, bool)> = out
            .iter()
            .map(|b| (b.name.as_str(), b.remote_only))
            .collect();
        assert_eq!(
            names,
            vec![("zebra", true), ("main", false), ("leaders", true)]
        );
    }

    #[test]
    fn filter_matches_name_and_subject_case_insensitively() {
        let all = vec![
            branch("main", "stable"),
            branch("feature/glyphs", "Glyph diagnostics"),
            branch("chatter", "Sidecar work"),
        ];
        assert_eq!(filter_branches(&all, "").len(), 3);
        assert_eq!(filter_branches(&all, "GLYPH").len(), 1);
        assert_eq!(filter_branches(&all, "chat").len(), 1);
        assert_eq!(filter_branches(&all, "zzz").len(), 0);
    }

    #[test]
    fn relative_time_buckets() {
        let now = 1_750_000_000;
        assert_eq!(relative_time(now - 10, now), "just now");
        assert_eq!(relative_time(now - 300, now), "5m ago");
        assert_eq!(relative_time(now - 7200, now), "2h ago");
        assert_eq!(relative_time(now - 3 * 86400, now), "3d ago");
        assert_eq!(relative_time(now - 90 * 86400, now), "3mo ago");
        assert_eq!(relative_time(now - 800 * 86400, now), "2y ago");
    }

    #[test]
    fn distance_text_covers_all_cases() {
        let base = RepoStatus {
            tip_ref: "origin/main".to_string(),
            ..Default::default()
        };
        assert!(distance_text(&base).contains("up to date"));
        let behind = RepoStatus {
            behind: 3,
            ..base.clone()
        };
        assert_eq!(distance_text(&behind), "3 commits behind origin/main");
        let one = RepoStatus {
            behind: 1,
            ..base.clone()
        };
        assert_eq!(distance_text(&one), "1 commit behind origin/main");
        let ahead = RepoStatus {
            ahead: 2,
            ..base.clone()
        };
        assert!(distance_text(&ahead).contains("ahead"));
        let both = RepoStatus {
            behind: 1,
            ahead: 1,
            ..base.clone()
        };
        assert!(distance_text(&both).contains("behind"));
        let detached = RepoStatus {
            detached: true,
            ..base
        };
        assert!(distance_text(&detached).contains("detached"));
    }

    #[test]
    fn refuses_suspicious_branch_names() {
        let repo = Path::new("C:\\nope");
        assert!(checkout(repo, "").is_err());
        assert!(checkout(repo, "a b").is_err());
        assert!(checkout(repo, "a\tb").is_err());
        assert!(checkout(repo, "a..b").is_err());
        // Leading dash would parse as a flag (`checkout -f` discards
        // local changes), `-` alone means "previous branch".
        assert!(checkout(repo, "-").is_err());
        assert!(checkout(repo, "-f").is_err());
        assert!(checkout(repo, "--orphan").is_err());
    }

    #[test]
    fn dedupe_drops_duplicate_remote_names() {
        let list = vec![remote("leaders"), remote("leaders"), branch("main", "")];
        let out = dedupe_branches(list);
        let names: Vec<(&str, bool)> = out
            .iter()
            .map(|b| (b.name.as_str(), b.remote_only))
            .collect();
        assert_eq!(names, vec![("leaders", true), ("main", false)]);
    }

    #[test]
    fn distance_text_notes_missing_remote_tip() {
        let local_only = RepoStatus {
            branch: "wip".to_string(),
            tip_ref: "wip".to_string(),
            ..Default::default()
        };
        assert!(distance_text(&local_only).contains("no remote tip"));
    }
}

/// End-to-end checks against scratch repos on disk (require `git`, a hard
/// runtime dependency of the launcher).
#[cfg(test)]
mod cli_tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn identify(dir: &Path) {
        git(dir, &["config", "user.email", "launcher-test@example.com"]);
        git(dir, &["config", "user.name", "Launcher Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_file(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "--quiet", "-m", &format!("add {name}")]);
    }

    struct Pair {
        _root: tempfile::TempDir,
        a: PathBuf,
        b: PathBuf,
    }

    /// Two clones (`a`, `b`) of a bare `origin.git`, both on `main` with
    /// upstream tracking and one shared commit.
    fn seed_pair() -> Pair {
        let root = tempfile::tempdir().unwrap();
        let rp = root.path();
        git(
            rp,
            &["init", "--bare", "--quiet", "-b", "main", "origin.git"],
        );
        let origin = rp.join("origin.git").to_string_lossy().to_string();
        for name in ["a", "b"] {
            git(
                rp,
                &[
                    "clone",
                    "--quiet",
                    "-c",
                    "protocol.file.allow=always",
                    &origin,
                    name,
                ],
            );
        }
        let a = rp.join("a");
        let b = rp.join("b");
        identify(&a);
        identify(&b);
        commit_file(&a, "mod.txt", "v1");
        git(&a, &["push", "--quiet", "-u", "origin", "main"]);
        git(&b, &["fetch", "--quiet", "origin"]);
        git(&b, &["checkout", "--quiet", "main"]);
        Pair { _root: root, a, b }
    }

    #[test]
    fn status_measures_behind_and_ahead() {
        let pair = seed_pair();
        commit_file(&pair.a, "a2.txt", "x");
        git(&pair.a, &["push", "--quiet", "origin", "main"]);
        commit_file(&pair.b, "b2.txt", "y");
        let st = status(&pair.b, true).unwrap();
        assert_eq!(st.branch, "main");
        assert_eq!(st.tip_ref, "origin/main");
        assert_eq!(st.behind, 1);
        assert_eq!(st.ahead, 1);
        assert!(st.fetch_error.is_none());
    }

    #[test]
    fn update_to_latest_fast_forwards_without_upstream() {
        let pair = seed_pair();
        commit_file(&pair.a, "a2.txt", "x");
        git(&pair.a, &["push", "--quiet", "origin", "main"]);
        // Deterministic no-upstream state regardless of DWIM defaults.
        git(&pair.b, &["branch", "-u", "origin/main"]);
        git(&pair.b, &["branch", "--unset-upstream"]);
        // No upstream: tip falls back to `origin/main`, still stale here.
        let before = status(&pair.b, false).unwrap();
        assert_eq!(before.tip_ref, "origin/main");
        assert_eq!((before.behind, before.ahead), (0, 0));
        update_to_latest(&pair.b).unwrap();
        let after = status(&pair.b, false).unwrap();
        assert_eq!((after.behind, after.ahead), (0, 0));
        assert!(pair.b.join("a2.txt").is_file());
    }

    #[test]
    fn checkout_remote_only_branch_creates_tracking_branch() {
        let pair = seed_pair();
        git(&pair.a, &["checkout", "--quiet", "-b", "frost"]);
        commit_file(&pair.a, "frost.txt", "snow");
        git(&pair.a, &["push", "--quiet", "-u", "origin", "frost"]);
        fetch(&pair.b).unwrap();
        let list = branches(&pair.b).unwrap();
        let frost = list.iter().find(|b| b.name == "frost").unwrap();
        assert!(frost.remote_only);
        checkout(&pair.b, "frost").unwrap();
        assert_eq!(current_branch(&pair.b).as_deref(), Some("frost"));
        assert!(pair.b.join("frost.txt").is_file());
    }
}
