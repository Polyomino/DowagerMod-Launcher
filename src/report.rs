//! Bug reports: redacted diagnostics filed as GitHub issues.
//!
//! Collection is local-only (no network): launcher sais git state, the
//! installer config, the launcher log tail, and the Civ4 log tails.
//! Everything passes through [`RedactCtx`] first — home paths, the user
//! name, the host name, and IP addresses come out as `<HOME>`, `<USER>`,
//! `<HOST>`, and `<IP>` so a report is safe to post publicly.
//!
//! Filing prefers the `gh` CLI (no token handling in the app); when `gh`
//! is missing or fails, the caller falls back to a prefilled
//! `issues/new` browser URL from [`issue_prefill_url`].

use std::path::Path;
use std::process::Stdio;

/// Where bug reports are filed.
pub const ISSUE_REPO: &str = "siegelh/DowagerMod";
/// Prefill URLs must stay short enough for browsers/proxies to swallow.
const MAX_PREFILL_BODY_CHARS: usize = 6000;
/// Per-section cap so one giant log can't bloat the issue body.
const MAX_SECTION_CHARS: usize = 8000;
/// Bytes read from the end of a log file before tailing its lines.
const MAX_LOG_READ_BYTES: u64 = 512 * 1024;

/// Case-insensitive substring replace (char-wise, so multi-byte text
/// around a match can never split a UTF-8 boundary).
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay: Vec<char> = haystack.chars().collect();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < hay.len() {
        let rest = &hay[i..];
        let hit = rest.len() >= needle.len()
            && rest[..needle.len()]
                .iter()
                .zip(needle.iter())
                .all(|(h, n)| h.to_lowercase().next() == Some(*n));
        if hit {
            out.push_str(replacement);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Machine-specific strings to scrub, longest pattern first so a full
/// home path wins over the bare user name it contains.
#[derive(Debug, Clone, Default)]
pub struct RedactCtx {
    patterns: Vec<(String, String)>,
}

impl RedactCtx {
    /// Gather user/host/home from this machine.
    pub fn current() -> Self {
        let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string());
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .or_else(|| {
                home.as_ref().and_then(|h| {
                    Path::new(h)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
            });
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok();
        Self::from_parts(home.as_deref(), user.as_deref(), host.as_deref())
    }

    /// Build from explicit parts (hermetic; used by tests).
    /// Bare names shorter than 3 chars are skipped — a 1–2 letter
    /// case-insensitive replace would eat ordinary words. Short names
    /// still vanish from paths via the generic `Users\<name>` scrub.
    fn from_parts(home: Option<&str>, user: Option<&str>, host: Option<&str>) -> Self {
        let mut patterns: Vec<(String, String)> = Vec::new();
        if let Some(h) = home.filter(|h| h.len() >= 3) {
            patterns.push((h.to_string(), "<HOME>".to_string()));
            let flipped = swap_separators(h);
            if flipped != h {
                patterns.push((flipped, "<HOME>".to_string()));
            }
        }
        if let Some(u) = user.filter(|u| u.len() >= 3) {
            patterns.push((u.to_string(), "<USER>".to_string()));
        }
        if let Some(h) = host.filter(|h| h.len() >= 3) {
            patterns.push((h.to_string(), "<HOST>".to_string()));
        }
        patterns.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        Self { patterns }
    }

    /// Scrub every known pattern, then unknown `Users\<name>` paths,
    /// then IP addresses.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (needle, replacement) in &self.patterns {
            out = replace_ci(&out, needle, replacement);
        }
        out = scrub_profile_names(&out);
        redact_ips(&out)
    }
}

/// Swap `\` and `/` so one home path covers both log spellings
/// (git prints forward slashes, tracebacks usually backslashes).
fn swap_separators(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '\\' => '/',
            '/' => '\\',
            _ => c,
        })
        .collect()
}

/// Fail-closed scrub for profile names this machine never told us
/// about: any remaining `Users\<name>` / `home/<name>` segment keeps
/// its prefix but loses the name. Well-known non-users (`Public`,
/// `Default`, `All Users`) are left alone.
fn scrub_profile_names(text: &str) -> String {
    let mut out = text.to_string();
    for prefix in ["Users\\", "Users/", "home/", "home\\"] {
        out = scrub_prefix_names(&out, prefix);
    }
    out
}

/// Single char-wise pass: keep the prefix's original spelling, then
/// the profile name — or `<USER>` when it names a real account.
fn scrub_prefix_names(text: &str, prefix: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = prefix.to_lowercase().chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let hit = chars.len() - i >= needle.len()
            && chars[i..i + needle.len()]
                .iter()
                .zip(needle.iter())
                .all(|(h, n)| h.to_lowercase().next() == Some(*n));
        if !hit {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        for c in &chars[i..i + needle.len()] {
            out.push(*c);
        }
        i += needle.len();
        let start = i;
        while i < chars.len()
            && (chars[i].is_alphanumeric() || matches!(chars[i], '.' | '_' | '-' | '$'))
        {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        if name.is_empty() || is_shared_profile(&name) {
            out.push_str(&name);
        } else {
            out.push_str("<USER>");
        }
    }
    out
}

fn is_shared_profile(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "public" | "default" | "defaultuser0" | "all"
    )
}

/// Redact IPv4 (`192.168.0.1`, optional `:port`) and IPv6-ish
/// (`::1`, `fe80::…`) tokens. Version-looking quads after the word
/// "version" (e.g. `version 3.19.0.0`) and timestamps (`12:00:00`)
/// are left alone: an IPv6 candidate must contain `::` or a hex
/// letter to qualify.
fn redact_ips(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    // Byte offset of the current token start, for the version-guard
    // lookbehind.
    let mut token_start: usize = 0;
    let mut offset: usize = 0;

    let flush = |out: &mut String, token: &mut String, text: &str, start: usize| {
        if is_ip_token(token) && !preceded_by_version_word(text, start) {
            out.push_str("<IP>");
            if let Some(port) = split_port(token) {
                out.push(':');
                out.push_str(port);
            }
        } else {
            out.push_str(token);
        }
        token.clear();
    };

    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == ':' {
            if token.is_empty() {
                token_start = offset;
            }
            token.push(c);
        } else {
            if !token.is_empty() {
                flush(&mut out, &mut token, text, token_start);
            }
            out.push(c);
        }
        offset += c.len_utf8();
    }
    if !token.is_empty() {
        flush(&mut out, &mut token, text, token_start);
    }
    out
}

fn is_ip_token(token: &str) -> bool {
    let bare = strip_port(token);
    is_ipv4(bare) || is_ipv6(bare)
}

/// Split a trailing `:port` (1–5 digits) off `host:port`, returning the
/// port when the host part still contains a dot (so bare IPv6 isn't
/// misread as host+port).
fn split_port(token: &str) -> Option<&str> {
    let (host, port) = token.rsplit_once(':')?;
    if host.contains('.')
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        Some(port)
    } else {
        None
    }
}

fn strip_port(token: &str) -> &str {
    match split_port(token) {
        Some(port) => &token[..token.len() - port.len() - 1],
        None => token,
    }
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u16>().unwrap_or(999) <= 255
    })
}

fn is_ipv6(s: &str) -> bool {
    let colons = s.bytes().filter(|b| *b == b':').count();
    if colons < 2 {
        return false;
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
    {
        return false;
    }
    // `::` or a hex letter: keeps `12:00:00` timestamps intact.
    s.contains("::") || s.bytes().any(|b| b.is_ascii_alphabetic())
}

/// True when the word right before `token_start` is a version word, so
/// `version 3.19.0.0` survives the IPv4 scrub.
fn preceded_by_version_word(text: &str, token_start: usize) -> bool {
    let head = &text[..token_start.min(text.len())];
    // Skip the separators between the word and the token first (the
    // space in `version 1.2.3.4`), then read the word itself.
    let mut rev = head.chars().rev().skip_while(|c| !c.is_alphanumeric());
    let word: String = rev.by_ref().take_while(|c| c.is_alphanumeric()).collect();
    let word: String = word.chars().rev().collect();
    let word = word.to_lowercase();
    word == "v" || word == "ver" || word.ends_with("version")
}

/// Last `max_lines` of `text` (whole file when shorter).
fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].join("\n")
}

/// First `max` chars + a truncation marker when longer.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…[truncated]");
    out
}

/// Read the tail of a log file without loading gigabytes: last
/// `MAX_LOG_READ_BYTES`, then the last `max_lines` of that window.
fn read_tail(path: &Path, max_lines: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(MAX_LOG_READ_BYTES as usize);
    let text = String::from_utf8_lossy(&bytes[start..]);
    // The window may start mid-line: drop the first partial line when
    // we sliced the file.
    let text = if start > 0 {
        text.split_once('\n').map(|(_, t)| t).unwrap_or(&text)
    } else {
        &text
    };
    Some(tail_lines(text, max_lines))
}

/// One diagnostics section: heading + preformatted body.
pub struct Section {
    pub heading: String,
    pub body: String,
}

/// Gather every diagnostics section, already redacted. Local-only: no
/// network fetch (a bug report must work offline — the browser
/// fallback needs the body even without `gh`).
pub fn collect(
    repo: Option<&Path>,
    live: crate::civ::LiveMatch,
    launcher_cfg: &crate::config::LauncherConfig,
) -> Vec<Section> {
    let ctx = RedactCtx::current();
    let mut sections = Vec::new();

    sections.push(Section {
        heading: "Environment".to_string(),
        body: format!(
            "launcher: {}\nos: {}\ntime: {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        ),
    });

    sections.push(Section {
        heading: "Mod checkout".to_string(),
        body: ctx.redact(&checkout_section(repo, live)),
    });

    let installer = crate::civ::installer_config_text()
        .unwrap_or_else(|| "not found (installer has not run yet?)".to_string());
    sections.push(Section {
        heading: "Installer config".to_string(),
        body: ctx.redact(&truncate_chars(&installer, MAX_SECTION_CHARS)),
    });

    let launcher = serde_json::to_string_pretty(launcher_cfg).unwrap_or_default();
    sections.push(Section {
        heading: "Launcher config".to_string(),
        body: ctx.redact(&launcher),
    });

    let log_tail = crate::log::path()
        .and_then(|p| read_tail(&p, 200))
        .unwrap_or_else(|| "launcher log unavailable".to_string());
    sections.push(Section {
        heading: "Launcher log (tail)".to_string(),
        body: ctx.redact(&truncate_chars(&log_tail, MAX_SECTION_CHARS)),
    });

    for (name, tail) in civ_log_tails() {
        sections.push(Section {
            heading: format!("Civ4 log: {name} (tail)"),
            body: ctx.redact(&truncate_chars(&tail, MAX_SECTION_CHARS)),
        });
    }
    sections
}

fn checkout_section(repo: Option<&Path>, live: crate::civ::LiveMatch) -> String {
    let Some(repo) = repo else {
        return "no checkout configured".to_string();
    };
    let mut out = format!("describe: {}\n", crate::git::describe(repo));
    match crate::git::status(repo, false) {
        Ok(st) => {
            out.push_str(&format!(
                "branch: {}{}\ncommit: {} {}\nvs tip: {}\n",
                st.branch,
                if st.detached { " (detached)" } else { "" },
                st.short_hash,
                st.subject,
                crate::git::distance_text(&st),
            ));
        }
        Err(e) => out.push_str(&format!("status failed: {e}\n")),
    }
    let live_text = match live {
        crate::civ::LiveMatch::Matches => "live install matches this checkout",
        crate::civ::LiveMatch::Differs => "live install differs from this checkout",
        crate::civ::LiveMatch::Unknown => "live match unknown",
    };
    out.push_str(&format!("live: {live_text}\n"));
    out
}

/// `(file name, tail)` for every Civ4 log, or a single placeholder
/// entry when the log folder is missing/empty.
fn civ_log_tails() -> Vec<(String, String)> {
    let Some(dir) = crate::civ::civ_log_dir() else {
        return vec![(
            "(missing)".to_string(),
            "Civ4 log folder not found on this machine".to_string(),
        )];
    };
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.to_lowercase().ends_with(".log") {
                names.push(name);
            }
        }
    }
    names.sort();
    if names.is_empty() {
        return vec![(
            "(empty)".to_string(),
            "Civ4 log folder has no .log files".to_string(),
        )];
    }
    names
        .into_iter()
        .map(|name| {
            let tail =
                read_tail(&dir.join(&name), 120).unwrap_or_else(|| "(unreadable)".to_string());
            (name, tail)
        })
        .collect()
}

/// Render the full issue body: the user's words first, then the
/// redacted diagnostics.
pub fn render_markdown(user_desc: &str, sections: &[Section]) -> String {
    let mut out = String::new();
    let desc = user_desc.trim();
    if desc.is_empty() {
        out.push_str("*(no description given)*\n");
    } else {
        out.push_str(desc);
        out.push('\n');
    }
    out.push_str(
        "\n---\n*Auto-collected by DowagerMod Launcher \
         (paths, host, and IP addresses redacted).*\n",
    );
    for s in sections {
        out.push_str(&format!("\n### {}\n```\n{}\n```\n", s.heading, s.body));
    }
    out
}

/// `gh` arguments for filing (pure constructor, for tests).
pub fn gh_issue_args(title: &str, body: &str) -> Vec<String> {
    vec![
        "issue".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        ISSUE_REPO.to_string(),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ]
}

/// File the issue via `gh`. Returns the created issue URL.
pub fn submit_via_gh(title: &str, body: &str) -> Result<String, String> {
    let args = gh_issue_args(title, body);
    run_gh("gh", &args)
}

fn run_gh(bin: &str, args: &[String]) -> Result<String, String> {
    // Resolved via the tool registry: a winget-installed `gh` is
    // invisible to our PATH until restart (bogus test names pass
    // through untouched — no override exists for them).
    let mut cmd = std::process::Command::new(crate::setup::tool_path(bin));
    crate::git::hide_child_console(&mut cmd);
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("could not run {bin} ({e}); is the GitHub CLI installed and on PATH?")
        })?;
    // Bounded wait: `gh` needs the network, which may hang.
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(120);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read as _;
                    let _ = out.read_to_string(&mut stdout);
                }
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = err.read_to_string(&mut stderr);
                }
                if !status.success() {
                    let detail = stderr
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("unknown gh error")
                        .to_string();
                    return Err(truncate_chars(&detail, 300));
                }
                return parse_issue_url(&stdout)
                    .ok_or_else(|| "gh succeeded but printed no issue URL".to_string());
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("gh timed out after 120s (offline?)".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed waiting on gh: {e}")),
        }
    }
}

/// First `http…` line of `gh issue create` output is the issue URL.
fn parse_issue_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http"))
        .map(str::to_string)
}

/// Prefilled `issues/new` URL for the browser fallback. The body is
/// truncated to stay URL-safe; the user reviews and submits by hand.
pub fn issue_prefill_url(title: &str, body: &str) -> String {
    let mut clipped = truncate_chars(body, MAX_PREFILL_BODY_CHARS);
    if clipped.len() < body.len() {
        clipped.push_str(
            "\n\n*(diagnostics truncated for the URL fallback \
            — attach the full report from the launcher's Copy button)*",
        );
    }
    format!(
        "https://github.com/{}/issues/new?title={}&body={}",
        ISSUE_REPO,
        url_encode(title),
        url_encode(&clipped)
    )
}

/// Minimal percent-encoding for query values (unreserved chars pass
/// through, everything else becomes `%XX`, spaces included).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RedactCtx {
        RedactCtx::from_parts(
            Some("C:\\Users\\Patryk"),
            Some("Patryk"),
            Some("PATRYK-DESKTOP"),
        )
    }

    #[test]
    fn home_path_redacted_before_bare_username() {
        let out = ctx().redact("repo: C:\\Users\\Patryk\\workspace\\DowagerMod");
        assert_eq!(out, "repo: <HOME>\\workspace\\DowagerMod");
    }

    #[test]
    fn redaction_is_case_insensitive_and_slash_agnostic() {
        let out = ctx().redact("see c:/users/patryk/mods and C:/USERS/PATRYK/x");
        assert_eq!(out, "see <HOME>/mods and <HOME>/x");
    }

    #[test]
    fn bare_username_and_host_redacted() {
        let out = ctx().redact("user Patryk on PATRYK-desktop crashed");
        assert_eq!(out, "user <USER> on <HOST> crashed");
    }

    #[test]
    fn unknown_profile_names_are_scrubbed_but_shared_ones_kept() {
        let out = ctx().redact("from C:\\Users\\SomeoneElse\\save and C:\\Users\\Public\\lib");
        assert!(out.contains("Users\\<USER>\\save"), "got: {out}");
        assert!(out.contains("Users\\Public\\lib"), "got: {out}");
    }

    #[test]
    fn short_names_skip_bare_replace_but_not_path_scrub() {
        let ctx = RedactCtx::from_parts(Some("C:\\Users\\Al"), Some("Al"), Some("PC"));
        // Bare "Al" everywhere would eat ordinary words ("already").
        assert_eq!(ctx.redact("already here, Al"), "already here, Al");
        // The known home path still scrubs via the long-pattern pass.
        assert_eq!(ctx.redact("at C:\\Users\\Al\\x"), "at <HOME>\\x");
        // ...and an unknown short profile name scrubs via the generic pass.
        let unknown = RedactCtx::from_parts(None, Some("Al"), Some("PC"));
        assert_eq!(
            unknown.redact("at C:\\Users\\Al\\x"),
            "at C:\\Users\\<USER>\\x"
        );
    }

    #[test]
    fn ipv4_and_port_redacted() {
        let out = ctx().redact("connect 192.168.1.10:2056 failed, also 10.0.0.5 ok");
        assert_eq!(out, "connect <IP>:2056 failed, also <IP> ok");
    }

    #[test]
    fn ipv6_redacted_but_timestamps_kept() {
        let out = ctx().redact("from fe80::1 at 12:00:00, loopback ::1");
        assert_eq!(out, "from <IP> at 12:00:00, loopback <IP>");
    }

    #[test]
    fn version_quads_survive_the_ip_scrub() {
        assert_eq!(
            ctx().redact("bts version 3.19.0.0 ready"),
            "bts version 3.19.0.0 ready"
        );
        // ...while the same quad in an address context does not.
        assert_eq!(ctx().redact("peer 3.19.0.4 dropped"), "peer <IP> dropped");
    }

    #[test]
    fn multibyte_text_around_matches_stays_valid() {
        let out = ctx().redact("żółć C:\\USERS\\patryk\\żółć end");
        assert_eq!(out, "żółć <HOME>\\żółć end");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn current_ctx_scrubs_the_real_home_dir() {
        let Some(home) = dirs::home_dir().map(|p| p.to_string_lossy().to_string()) else {
            return;
        };
        if home.len() < 3 {
            return;
        }
        let out = RedactCtx::current().redact(&format!("at {home}\\file.txt"));
        assert!(!out.contains(&home), "home leaked: {out}");
        assert!(out.contains("<HOME>"), "got: {out}");
    }

    #[test]
    fn tails_and_truncation_behave() {
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
        assert_eq!(tail_lines("", 5), "");
        let long = "x".repeat(100);
        let clipped = truncate_chars(&long, 10);
        assert!(clipped.starts_with(&"x".repeat(10)), "got: {clipped}");
        assert!(clipped.contains("truncated"), "got: {clipped}");
    }

    #[test]
    fn read_tail_reads_the_end_not_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.log");
        let mut body = String::new();
        for i in 0..50 {
            body.push_str(&format!("line {i:02}\n"));
        }
        std::fs::write(&file, &body).unwrap();
        let tail = read_tail(&file, 3).unwrap();
        assert_eq!(tail, "line 47\nline 48\nline 49");
        assert!(read_tail(&dir.path().join("missing.log"), 3).is_none());
    }

    #[test]
    fn markdown_renders_desc_then_sections() {
        let sections = vec![Section {
            heading: "Env".to_string(),
            body: "x".to_string(),
        }];
        let md = render_markdown("it broke", &sections);
        assert!(md.starts_with("it broke\n"), "got: {md}");
        assert!(md.contains("### Env"), "got: {md}");
        assert!(md.contains("redacted"), "got: {md}");
        let empty = render_markdown("  ", &[]);
        assert!(empty.contains("no description"), "got: {empty}");
    }

    #[test]
    fn gh_args_target_the_mod_repo() {
        let args = gh_issue_args("t", "b");
        assert_eq!(
            &args[..4],
            &[
                "issue".to_string(),
                "create".to_string(),
                "--repo".to_string(),
                ISSUE_REPO.to_string(),
            ]
        );
        assert!(args.contains(&"t".to_string()));
    }

    #[test]
    fn gh_submit_fails_cleanly_when_gh_is_missing() {
        // Bogus binary name: hermetic by construction. (A PATH override
        // is NOT — Windows still resolved the real `gh` past it, and an
        // earlier version of this test filed two live issues. Never run
        // the real `gh issue create` in tests.)
        let err = run_gh("dml-no-such-gh-binary-xyz", &gh_issue_args("t", "b")).unwrap_err();
        assert!(err.contains("dml-no-such-gh-binary-xyz"), "got: {err}");
    }

    #[test]
    fn gh_url_parsing_takes_first_http_line() {
        assert_eq!(
            parse_issue_url("https://github.com/o/r/issues/1\n"),
            Some("https://github.com/o/r/issues/1".to_string())
        );
        assert_eq!(parse_issue_url("no url here\n"), None);
    }

    #[test]
    fn prefill_url_encodes_and_clips() {
        let url = issue_prefill_url("a b", "line1\nline2");
        assert!(
            url.starts_with("https://github.com/siegelh/DowagerMod/issues/new?title=a%20b&body="),
            "got: {url}"
        );
        assert!(url.contains("line1%0Aline2"), "got: {url}");
        let big = "y".repeat(MAX_PREFILL_BODY_CHARS + 100);
        let clipped = issue_prefill_url("t", &big);
        assert!(clipped.contains("truncated"), "url not clipped");
        assert!(
            clipped.len() < big.len() + 500,
            "url far too long: {}",
            clipped.len()
        );
    }

    #[test]
    fn collect_without_repo_still_redacts() {
        let cfg = crate::config::LauncherConfig::default();
        let sections = collect(None, crate::civ::LiveMatch::Unknown, &cfg);
        assert!(sections.len() >= 5, "got {}", sections.len());
        assert!(sections.iter().any(|s| s.heading == "Environment"));
        assert!(sections.iter().any(|s| s.heading == "Mod checkout"));
        // Whatever the machine contributed must already be scrubbed.
        let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string());
        if let Some(home) = home.filter(|h| h.len() >= 10) {
            for s in &sections {
                assert!(
                    !replace_ci(&s.body, &home, "LEAK").contains("LEAK"),
                    "home leaked in {}: {}",
                    s.heading,
                    s.body
                );
            }
        }
    }
}
