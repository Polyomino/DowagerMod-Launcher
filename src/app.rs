//! eframe/egui front end: status card, searchable branch list, update/deploy/launch.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::LauncherConfig;
use crate::git::{self, BranchInfo, RepoStatus};
use crate::{civ, config, report, setup, update};

/// Warm bronze accent — Civ-appropriate, and deliberately not the
/// purple/blue default.
const BRONZE: egui::Color32 = egui::Color32::from_rgb(0xC9, 0xA2, 0x27);
const BRONZE_LIGHT: egui::Color32 = egui::Color32::from_rgb(0xE3, 0xC0, 0x5A);
const INK_ON_BRONZE: egui::Color32 = egui::Color32::from_rgb(0x20, 0x18, 0x06);
const OK_GREEN: egui::Color32 = egui::Color32::from_rgb(0x7B, 0xC7, 0x6E);
const WARN_AMBER: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xA8, 0x3C);
const ERR_RED: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x6C, 0x5B);

/// Message from a background worker thread to the UI thread.
/// Every variant but `Deployed` carries the job epoch: results from an
/// older epoch (orphaned by a newer spawn or a repo switch) are dropped.
enum JobOut {
    /// Full refresh finished: status + branches + live-match, or an error.
    Refreshed(
        u64,
        Result<(RepoStatus, Vec<BranchInfo>, civ::LiveMatch), String>,
    ),
    /// Fast-forward to the remote tip finished.
    Updated(u64, Result<String, String>),
    /// `git checkout` finished.
    CheckedOut(u64, Result<String, String>),
    /// Installer child process exited (deploy cannot overlap anything,
    /// so no epoch is needed).
    Deployed,
    /// Bug-report submit finished: issue URL / fallback note, or a reason.
    /// Independent of repo switches (it files a snapshot), so no epoch.
    Reported(Result<String, String>),
    /// Update check finished: a newer release, up-to-date, or a reason.
    /// No epoch (nothing about the repo can orphan it).
    UpdateCheck(Result<Option<update::ReleaseInfo>, String>),
    /// Update download/apply failed (success exits the process, so only
    /// failure needs a message).
    UpdateFailed(String),
    /// Native file dialog closed: the picked path, or `None` on cancel.
    /// No epoch (a pick can't be orphaned by repo activity).
    Picked(PickTarget, Option<PathBuf>),
    /// Git ensure finished: how to spawn it, or a reason. No epoch.
    GitEnsured(Result<PathBuf, String>),
    /// `git clone` finished. No epoch (only one clone runs at a time,
    /// behind `busy`).
    Cloned(Result<String, String>),
    /// `gh` ensure finished. No epoch.
    GhEnsured(Result<(), String>),
    /// `gh auth login` terminal closed: authed now, or a reason.
    GhAuthed(Result<(), String>),
}

/// What a native file dialog is picking for: a Settings field, the
/// empty-state Find flow (adopted + saved on pick), or the clone
/// destination field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    RepoDir,
    CivExe,
    FindRepo,
    CloneDest,
}

/// Guided `gh` setup inside the Report dialog: install, then sign in.
/// Only appears once a submit discovers gh missing or unauthenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum GhFlow {
    #[default]
    Idle,
    Missing,
    Installing,
    NeedsAuth,
    Authing,
}

/// Starting folder for a Browse dialog: the draft dir itself when it
/// exists, else its parent, else the dialog default.
fn pick_start_dir(draft: &str) -> Option<PathBuf> {
    let trimmed = draft.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = PathBuf::from(trimmed);
    if p.is_dir() {
        return Some(p);
    }
    p.parent().filter(|p| p.is_dir()).map(Path::to_path_buf)
}

/// One-time theme setup: bronze selection, roomier spacing.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(0xC9, 0xA2, 0x27, 0x3C);
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, BRONZE);
    style.visuals.hyperlink_color = BRONZE_LIGHT;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.indent = 18.0;
    ctx.set_style(style);
}

/// The one headline action per section: bronze fill, dark text.
/// An explicit dimmed look when disabled (an explicit `.fill` would
/// otherwise stay bright under `add_enabled(false, …)`).
fn primary_button(text: &str, enabled: bool) -> egui::Button<'_> {
    let (fill, ink) = if enabled {
        (BRONZE, INK_ON_BRONZE)
    } else {
        (
            egui::Color32::from_rgb(0x4A, 0x40, 0x22),
            egui::Color32::from_rgb(0x8A, 0x80, 0x66),
        )
    };
    egui::Button::new(egui::RichText::new(text).color(ink).strong())
        .fill(fill)
        .min_size(egui::vec2(160.0, 32.0))
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(title).size(16.0).strong());
    ui.add_space(4.0);
}

/// Color for the behind/ahead line: green when synced, amber when behind,
/// red when detached (nothing sane to update to).
fn distance_color(st: &RepoStatus) -> egui::Color32 {
    if st.detached {
        ERR_RED
    } else if st.behind > 0 {
        WARN_AMBER
    } else {
        OK_GREEN
    }
}

/// Clamp a Up/Down selection move into `0..count`. Returns the new
/// index plus whether it actually moved (drives one-shot list scroll).
fn move_selection(selected: usize, count: usize, down: bool) -> (usize, bool) {
    if count == 0 {
        return (0, false);
    }
    let selected = selected.min(count - 1);
    let next = if down {
        (selected + 1).min(count - 1)
    } else {
        selected.saturating_sub(1)
    };
    (next, next != selected)
}

/// Checkout is a no-op when already on the target branch.
fn needs_checkout(current_branch: Option<&str>, target: &str) -> bool {
    current_branch != Some(target)
}

/// Settings update row: the launcher version always, plus the last
/// check status once one exists (a status must never hide the version).
fn update_row_text(version: &str, status: &str) -> String {
    if status.is_empty() {
        format!("launcher v{version}")
    } else {
        format!("launcher v{version} · {status}")
    }
}

/// Error sniffing so failures stand out in the log line. Matches our
/// own failure prefixes plus git's `error:`/`fatal:` markers — deliberately
/// NOT bare substrings like "error", which also appear in success output
/// (e.g. a fast-forward listing `error_handler.py`). `:` is illegal in
/// Windows filenames, so the colon markers can't false-positive there.
fn looks_like_error(log: &str) -> bool {
    let lower = log.to_lowercase();
    [
        "update failed",
        "switch failed",
        "refresh failed",
        "fetch failed",
        "report failed",
        "update failed",
        "install failed",
        "clone failed",
        "not a dowagermod checkout",
        "save failed",
        "failed to ",
        "failed:",
        "error:",
        "fatal:",
        "refus",
        "timed out",
        "not found",
        "not a git checkout",
        "no installer found",
        "no remote tip",
        "not on a branch",
        "no mod checkout",
        "already running",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// One branch row: name prominent (bronze when current), hash/time/subject
/// dimmed, remote tag italic. Single-line `LayoutJob` so the whole row
/// stays one selectable unit.
fn branch_row_job(b: &BranchInfo, age: &str, ui: &egui::Ui) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let weak = ui.visuals().weak_text_color();
    let mut job = LayoutJob::default();
    job.append(
        &b.name,
        0.0,
        TextFormat {
            font_id: egui::FontId::proportional(13.5),
            color: if b.is_current {
                BRONZE_LIGHT
            } else {
                ui.visuals().strong_text_color()
            },
            ..Default::default()
        },
    );
    job.append(
        &format!("  {}", b.short_hash),
        0.0,
        TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: weak,
            ..Default::default()
        },
    );
    job.append(
        &format!("  {age}  {}", b.subject),
        0.0,
        TextFormat {
            font_id: egui::FontId::proportional(12.0),
            color: weak,
            ..Default::default()
        },
    );
    if b.is_current {
        // Bronze name alone is subtle: tag it so the checkout stands out
        // wherever it sits in newest-first order.
        job.append(
            "  [current]",
            0.0,
            TextFormat {
                font_id: egui::FontId::proportional(12.0),
                color: weak,
                italics: true,
                ..Default::default()
            },
        );
    }
    if b.remote_only {
        job.append(
            "  [remote]",
            0.0,
            TextFormat {
                font_id: egui::FontId::proportional(12.0),
                color: weak,
                italics: true,
                ..Default::default()
            },
        );
    }
    job
}

pub struct LauncherApp {
    config: LauncherConfig,
    repo: Option<PathBuf>,
    status: Option<RepoStatus>,
    branches: Vec<BranchInfo>,
    search: String,
    selected: usize,
    log: String,
    busy: Option<String>,
    show_settings: bool,
    draft_repo_path: String,
    draft_app_id: String,
    draft_exe: String,
    focused_search: bool,
    job_epoch: u64,
    /// Set when keyboard nav (or a filter change) moves the selection:
    /// the list scrolls to the row exactly once, then this clears. Mouse
    /// scrolling never sets it, so the view can't snap back mid-scroll.
    scroll_to_selected: bool,
    /// Set when a checkout completes: the follow-up refresh selects the
    /// new current branch and scrolls it into view.
    scroll_to_current: bool,
    /// An installer child is running: no second deploy, no launch, and no
    /// git jobs (the payload must not change mid-install).
    deploying: bool,
    live_match: civ::LiveMatch,
    show_report: bool,
    report_title: String,
    report_desc: String,
    report_preview: Option<String>,
    report_sending: bool,
    report_result: String,
    update_offer: Option<update::ReleaseInfo>,
    update_checking: bool,
    update_downloading: bool,
    update_status: String,
    update_error: String,
    /// A native file dialog is open for this target (Browse buttons off).
    picking: Option<PickTarget>,
    git_missing: bool,
    git_installing: bool,
    git_error: String,
    show_clone: bool,
    clone_dest: String,
    clone_error: String,
    find_error: String,
    gh_flow: GhFlow,
    gh_message: String,
    tx: Sender<JobOut>,
    rx: Receiver<JobOut>,
}

impl LauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_style(&cc.egui_ctx);
        let mut config = LauncherConfig::load();
        let mut repo = config.repo_path();
        // Demo/test hook: simulate a true first run (no configured
        // path either, so the header reads like one).
        if std::env::var("DML_NO_REPO").as_deref() == Ok("1") {
            config.mod_repo_path = String::new();
            repo = None;
        }
        // Draft mirrors the stored value verbatim: empty means auto-detect.
        // (Prefilling the detected path here would freeze auto-detect into
        // an explicit path on the next Save.)
        let draft_repo_path = config.mod_repo_path.clone();
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            draft_app_id: config.steam_app_id.to_string(),
            draft_exe: config.civ_exe_override.clone(),
            config,
            repo,
            status: None,
            branches: Vec::new(),
            search: String::new(),
            selected: 0,
            log: String::new(),
            busy: None,
            show_settings: false,
            draft_repo_path,
            focused_search: false,
            job_epoch: 0,
            scroll_to_selected: false,
            scroll_to_current: false,
            deploying: false,
            live_match: civ::LiveMatch::Unknown,
            show_report: false,
            report_title: String::new(),
            report_desc: String::new(),
            report_preview: None,
            report_sending: false,
            report_result: String::new(),
            update_offer: None,
            update_checking: false,
            update_downloading: false,
            update_status: String::new(),
            update_error: String::new(),
            picking: None,
            git_missing: false,
            git_installing: false,
            git_error: String::new(),
            show_clone: false,
            clone_dest: String::new(),
            clone_error: String::new(),
            find_error: String::new(),
            gh_flow: GhFlow::Idle,
            gh_message: String::new(),
            tx,
            rx,
        };
        // Tool check first: without git every repo call fails, so skip
        // the sync snapshot and install it in the background instead.
        // (One fast `--version` spawn; a missing binary fails fast.)
        app.git_missing = !setup::tool_usable("git");
        if app.git_missing {
            app.spawn_git_install();
        } else if let Some(repo) = app.ready_repo() {
            // Fast first paint: synchronous local-only snapshot (no
            // network fetch), then a background thread does fetch +
            // full refresh. Skipped without a ready checkout (no
            // pointless failure lines on the selection screen).
            match git::status(&repo, false) {
                Ok(st) => app.status = Some(st),
                Err(e) => app.set_log(e),
            }
            if let Ok(list) = git::branches(&repo) {
                app.branches = list;
            }
            app.spawn_refresh(true);
        }
        // No repo: the empty-state screen offers Find / Checkout (no
        // auto-open Settings anymore).
        // Self-update check in the background (skipped for dev builds).
        if update::should_check() {
            app.spawn_update_check();
        }
        app
    }

    fn spawn_refresh(&mut self, fetch_first: bool) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.busy = Some(if fetch_first {
            "fetching…".to_string()
        } else {
            "refreshing…".to_string()
        });
        self.job_epoch += 1;
        let epoch = self.job_epoch;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = git::status(&repo, fetch_first)
                .and_then(|st| git::branches(&repo).map(|list| (st, list, civ::live_match(&repo))));
            let _ = tx.send(JobOut::Refreshed(epoch, out));
        });
    }

    fn spawn_update(&mut self) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.busy = Some("updating…".to_string());
        self.job_epoch += 1;
        let epoch = self.job_epoch;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(JobOut::Updated(epoch, git::update_to_latest(&repo)));
        });
    }

    fn spawn_checkout(&mut self, branch: String) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.busy = Some(format!("switching to {branch}…"));
        self.job_epoch += 1;
        let epoch = self.job_epoch;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(JobOut::CheckedOut(epoch, git::checkout(&repo, &branch)));
        });
    }

    fn spawn_deploy(&mut self) {
        if self.deploying || self.busy.is_some() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            self.set_log("no mod checkout configured".to_string());
            return;
        };
        // Cross-process single-flight first: a second launcher window may
        // be mid-deploy (the in-process `deploying` flag can't see it).
        let Some(lock) = civ::acquire_deploy_lock() else {
            self.set_log(
                "installer already running in another window — wait for it to finish".to_string(),
            );
            return;
        };
        match civ::deploy(&repo) {
            Ok(child) => {
                self.deploying = true;
                self.set_log("installer started (approve the UAC prompt)".to_string());
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let mut child = child;
                    let _ = child.wait();
                    drop(lock);
                    let _ = tx.send(JobOut::Deployed);
                });
            }
            Err(e) => self.set_log(e),
        }
    }

    fn spawn_report(&mut self, title: String, desc: String) {
        if self.report_sending {
            return;
        }
        self.report_sending = true;
        self.report_result.clear();
        let repo = self.repo.clone();
        let live = self.live_match;
        let cfg = self.config.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let sections = report::collect(repo.as_deref(), live, &cfg);
            let body = report::render_markdown(&desc, &sections);
            let outcome = match report::submit_via_gh(&title, &body) {
                Ok(url) => Ok(format!("issue filed: {url}")),
                Err(e) => {
                    // Fallback: a prefilled browser form, one review + click.
                    let url = report::issue_prefill_url(&title, &body);
                    match open::that(&url) {
                        Ok(()) => Ok(format!(
                            "gh unavailable ({e}) — opened prefilled issue in browser"
                        )),
                        Err(be) => Err(format!("{e} (browser fallback also failed: {be})")),
                    }
                }
            };
            let _ = tx.send(JobOut::Reported(outcome));
        });
    }

    /// Synchronous collect + render for Preview / Copy (local-only, fast).
    fn build_report_body(&self) -> String {
        let sections = report::collect(self.repo.as_deref(), self.live_match, &self.config);
        report::render_markdown(&self.report_desc, &sections)
    }

    fn spawn_update_check(&mut self) {
        if self.update_checking || self.update_downloading {
            return;
        }
        self.update_checking = true;
        self.update_status = "checking…".to_string();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let local = update::local_version();
            let out = match update::fetch_latest() {
                Ok(rel) if update::is_newer(&local, &rel.tag) => Ok(Some(rel)),
                Ok(_) => Ok(None),
                Err(e) => Err(e),
            };
            let _ = tx.send(JobOut::UpdateCheck(out));
        });
    }

    fn spawn_update_download(&mut self, rel: update::ReleaseInfo) {
        if self.update_downloading {
            return;
        }
        // The offer dialog can appear in a dev build via the manual
        // Settings check: never let it swap a dev binary.
        if !update::should_check() {
            self.update_error = "self-update is disabled for dev builds".to_string();
            return;
        }
        self.update_downloading = true;
        self.update_error.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let outcome = (|| -> Result<(), String> {
                let exe = std::env::current_exe()
                    .map_err(|e| format!("cannot locate running exe: {e}"))?;
                let pending = update::pending_path(&exe);
                update::download(&rel.exe_url, &pending)?;
                update::spawn_updater(&exe, &pending)?;
                Ok(())
            })();
            match outcome {
                // Swap staged: exit at once so the updater can move the exe.
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    let _ = tx.send(JobOut::UpdateFailed(e));
                }
            }
        });
    }

    /// Open a native Browse dialog on a worker thread (the sync dialog
    /// pumps its own loop; running it on the UI thread would stall
    /// repaints behind it). The pick lands in the draft field and still
    /// needs Save, same as typing it in.
    fn spawn_pick(&mut self, target: PickTarget) {
        if self.picking.is_some() {
            return;
        }
        self.picking = Some(target);
        let draft = match target {
            PickTarget::RepoDir | PickTarget::FindRepo => self.draft_repo_path.clone(),
            PickTarget::CivExe => self.draft_exe.clone(),
            PickTarget::CloneDest => self.clone_dest.clone(),
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut dlg = rfd::FileDialog::new();
            if let Some(dir) = pick_start_dir(&draft) {
                dlg = dlg.set_directory(dir);
            }
            let picked = match target {
                PickTarget::RepoDir | PickTarget::FindRepo | PickTarget::CloneDest => {
                    dlg.pick_folder()
                }
                PickTarget::CivExe => dlg.add_filter("Executable", &["exe"]).pick_file(),
            };
            let _ = tx.send(JobOut::Picked(target, picked));
        });
    }

    fn spawn_git_install(&mut self) {
        if self.git_installing {
            return;
        }
        self.git_installing = true;
        self.git_error.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(JobOut::GitEnsured(setup::ensure_tool(
                "git",
                setup::GIT_WINGET_ID,
            )));
        });
    }

    fn spawn_clone(&mut self, dest: String) {
        if self.busy.is_some() || self.deploying || self.git_missing {
            return;
        }
        self.busy = Some("cloning…".to_string());
        self.clone_error.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = git::clone_repo(git::MOD_REPO_URL, Path::new(&dest));
            let _ = tx.send(JobOut::Cloned(out));
        });
    }

    fn spawn_gh_install(&mut self) {
        if !matches!(self.gh_flow, GhFlow::Missing) {
            return;
        }
        self.gh_flow = GhFlow::Installing;
        self.gh_message.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = setup::ensure_tool("gh", setup::GH_WINGET_ID).map(|_| ());
            let _ = tx.send(JobOut::GhEnsured(out));
        });
    }

    fn spawn_gh_auth(&mut self) {
        if !matches!(self.gh_flow, GhFlow::NeedsAuth) {
            return;
        }
        self.gh_flow = GhFlow::Authing;
        self.gh_message.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Visible console on purpose: `gh auth login` is interactive
            // (browser/device code the user completes in the terminal).
            let out = match std::process::Command::new(setup::tool_path("gh"))
                .arg("auth")
                .arg("login")
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    if setup::gh_authed() {
                        Ok(())
                    } else {
                        Err(
                            "authentication incomplete — finish signing in and try again"
                                .to_string(),
                        )
                    }
                }
                Err(e) => Err(format!("could not start gh auth: {e}")),
            };
            let _ = tx.send(JobOut::GhAuthed(out));
        });
    }

    /// Submit via gh when possible; otherwise steer into the guided
    /// install/auth flow (which auto-submits once gh is ready).
    fn submit_or_setup_gh(&mut self, title: String, desc: String) {
        if !setup::tool_usable("gh") {
            self.gh_flow = GhFlow::Missing;
            self.gh_message.clear();
            return;
        }
        if !setup::gh_authed() {
            self.gh_flow = GhFlow::NeedsAuth;
            self.gh_message.clear();
            return;
        }
        self.gh_flow = GhFlow::Idle;
        self.spawn_report(title, desc);
    }

    /// The checkout when one is ready to manage: configured AND a
    /// valid DowagerMod checkout. Anything else (missing, not a repo,
    /// wrong folder) shows the Find / Checkout selection screen instead
    /// — including stale configs saved before validation existed
    /// (self-healing). A few fs stats per call; negligible.
    fn ready_repo(&self) -> Option<PathBuf> {
        self.repo.clone().filter(|r| git::validate_repo(r).is_ok())
    }

    /// Adopt a repo path (Find / Clone flows): validate, persist,
    /// reset repo state, refresh. Invalid picks never touch the config —
    /// the caller shows the error and stays on the selection screen.
    fn adopt_repo(&mut self, path: PathBuf) -> Result<(), String> {
        git::validate_repo(&path)?;
        self.config.mod_repo_path = path.to_string_lossy().to_string();
        self.config
            .save()
            .map_err(|e| format!("save failed: {e}"))?;
        self.draft_repo_path = self.config.mod_repo_path.clone();
        self.repo = Some(path);
        self.job_epoch += 1;
        self.status = None;
        self.branches.clear();
        self.selected = 0;
        self.spawn_refresh(true);
        Ok(())
    }

    fn poll_jobs(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                JobOut::Refreshed(epoch, Ok((mut st, list, live))) => {
                    if epoch != self.job_epoch {
                        continue; // stale: orphaned by a newer spawn
                    }
                    // Local-only refreshes (after update/checkout) carry no
                    // fetch info: keep the previous fetch state instead of
                    // blanking it.
                    if st.fetched_at.is_none() && st.fetch_error.is_none() {
                        if let Some(prev) = &self.status {
                            st.fetched_at = prev.fetched_at;
                            st.fetch_error.clone_from(&prev.fetch_error);
                        }
                    }
                    // Keep the selected branch by name when it is still listed.
                    let keep = self.filtered().get(self.selected).map(|b| b.name.clone());
                    self.status = Some(st);
                    self.branches = list;
                    self.live_match = live;
                    let fallback = keep
                        .and_then(|name| self.filtered().iter().position(|b| b.name == name))
                        .unwrap_or(0);
                    if self.scroll_to_current {
                        self.scroll_to_current = false;
                        // Follow the fresh checkout; if a filter hides the
                        // current branch, keep the old selection instead.
                        if let Some(i) = self.filtered().iter().position(|b| b.is_current) {
                            self.selected = i;
                            self.scroll_to_selected = true;
                        } else {
                            self.selected = fallback;
                        }
                    } else {
                        self.selected = fallback;
                    }
                    self.busy = None;
                }
                JobOut::Refreshed(epoch, Err(e)) => {
                    if epoch != self.job_epoch {
                        continue; // stale: orphaned by a newer spawn
                    }
                    self.set_log(format!("refresh failed: {e}"));
                    self.busy = None;
                }
                JobOut::Updated(epoch, Ok(out)) => {
                    if epoch != self.job_epoch {
                        continue;
                    }
                    self.set_log(if out.is_empty() {
                        "already up to date".to_string()
                    } else {
                        out
                    });
                    self.spawn_refresh(false);
                }
                JobOut::Updated(epoch, Err(e)) => {
                    if epoch != self.job_epoch {
                        continue;
                    }
                    self.set_log(format!("update failed: {e}"));
                    self.busy = None;
                }
                JobOut::CheckedOut(epoch, Ok(_)) => {
                    if epoch != self.job_epoch {
                        continue;
                    }
                    self.set_log("branch switched".to_string());
                    self.scroll_to_current = true;
                    self.spawn_refresh(false);
                }
                JobOut::CheckedOut(epoch, Err(e)) => {
                    if epoch != self.job_epoch {
                        continue;
                    }
                    self.set_log(format!("switch failed: {e}"));
                    self.busy = None;
                }
                JobOut::Deployed => {
                    self.deploying = false;
                    self.set_log("installer window closed".to_string());
                    // Recompute the live-match line (deploy rewrote it).
                    self.spawn_refresh(false);
                }
                JobOut::Reported(Ok(msg)) => {
                    self.report_sending = false;
                    self.report_result = msg.clone();
                    self.set_log(msg);
                }
                JobOut::Reported(Err(e)) => {
                    self.report_sending = false;
                    self.set_log(format!("report failed: {e}"));
                    self.report_result = format!("report failed: {e}");
                }
                JobOut::UpdateCheck(Ok(Some(rel))) => {
                    self.update_checking = false;
                    self.update_status = format!("{} available", rel.tag);
                    self.update_offer = Some(rel);
                }
                JobOut::UpdateCheck(Ok(None)) => {
                    self.update_checking = false;
                    self.update_status =
                        format!("you're on the latest (v{})", update::local_version());
                }
                JobOut::UpdateCheck(Err(e)) => {
                    self.update_checking = false;
                    self.update_status = format!("last check failed: {e}");
                }
                JobOut::UpdateFailed(e) => {
                    self.update_downloading = false;
                    self.set_log(format!("update failed: {e}"));
                    self.update_error = format!("update failed: {e}");
                }
                JobOut::Picked(target, picked) => {
                    self.picking = None;
                    let Some(path) = picked else {
                        return; // Cancel leaves everything untouched.
                    };
                    match target {
                        PickTarget::RepoDir => {
                            // A pick still needs Save, same as typing it in.
                            self.draft_repo_path = path.to_string_lossy().to_string();
                        }
                        PickTarget::CivExe => {
                            self.draft_exe = path.to_string_lossy().to_string();
                        }
                        PickTarget::CloneDest => {
                            self.clone_dest = path.to_string_lossy().to_string();
                        }
                        PickTarget::FindRepo => {
                            match self.adopt_repo(path) {
                                Ok(()) => {
                                    self.find_error.clear();
                                    self.set_log("checkout selected — refreshing…".to_string());
                                }
                                Err(e) => {
                                    // Back to the selection screen with
                                    // the reason (config untouched).
                                    self.set_log(e.clone());
                                    self.find_error = e;
                                }
                            }
                        }
                    }
                }
                JobOut::GitEnsured(Ok(path)) => {
                    self.git_installing = false;
                    self.git_missing = false;
                    if path.as_os_str() == "git" {
                        self.set_log("git ready".to_string());
                    } else {
                        self.set_log(format!("git installed and ready ({})", path.display()));
                    }
                    // The initial snapshot was skipped without git.
                    self.spawn_refresh(true);
                }
                JobOut::GitEnsured(Err(e)) => {
                    self.git_installing = false;
                    self.set_log(format!("install failed: {e}"));
                    self.git_error = e;
                }
                JobOut::Cloned(Ok(msg)) => {
                    self.busy = None;
                    self.set_log(msg);
                    // The clone is an explicit switch request: adopt it
                    // even over an existing checkout (last action wins).
                    // A surprise-invalid clone keeps the dialog open.
                    match self.adopt_repo(PathBuf::from(self.clone_dest.trim())) {
                        Ok(()) => self.show_clone = false,
                        Err(e) => {
                            self.set_log(e.clone());
                            self.clone_error = e;
                        }
                    }
                }
                JobOut::Cloned(Err(e)) => {
                    self.busy = None;
                    self.set_log(format!("clone failed: {e}"));
                    self.clone_error = e;
                }
                JobOut::GhEnsured(Ok(())) => {
                    self.gh_flow = GhFlow::NeedsAuth;
                    self.gh_message.clear();
                }
                JobOut::GhEnsured(Err(e)) => {
                    self.gh_flow = GhFlow::Missing;
                    self.gh_message = e;
                }
                JobOut::GhAuthed(Ok(())) => {
                    self.gh_flow = GhFlow::Idle;
                    self.gh_message.clear();
                    // Signed in: continue straight into the submit the
                    // user originally asked for.
                    let title = self.report_title.trim().to_string();
                    if title.is_empty() {
                        self.report_result = "signed in — submit when ready".to_string();
                    } else {
                        self.spawn_report(title, self.report_desc.clone());
                    }
                }
                JobOut::GhAuthed(Err(e)) => {
                    self.gh_flow = GhFlow::NeedsAuth;
                    self.gh_message = e;
                }
            }
        }
    }

    fn filtered(&self) -> Vec<&BranchInfo> {
        git::filter_branches(&self.branches, &self.search)
    }

    /// Status line for the UI bar, mirrored to the session log file.
    fn set_log(&mut self, msg: String) {
        crate::log::append(&msg);
        self.log = msg;
    }

    /// Open the clone dialog with a fresh destination (shared by the
    /// empty-state screen and the Installed card button).
    fn open_clone_dialog(&mut self) {
        self.clone_dest = config::default_clone_dir().to_string_lossy().to_string();
        self.clone_error.clear();
        self.show_clone = true;
    }

    fn checkout_selected(&mut self) {
        // Enter/double-click bypass the disabled-button gate, so guard here:
        // overlapping git jobs fight over the repo lock, and the payload
        // must not change while the installer reads it.
        if self.busy.is_some() || self.deploying || self.git_missing {
            return;
        }
        if let Some(b) = self.filtered().get(self.selected).map(|b| b.name.clone()) {
            let current = self.status.as_ref().map(|s| s.branch.as_str());
            if !needs_checkout(current, &b) {
                self.set_log(format!("already on {b}"));
                return;
            }
            self.spawn_checkout(b);
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs();
        if self.busy.is_some() || self.deploying {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // Global refresh hotkey (mirrors the header button's gate).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::R))
            && self.busy.is_none()
            && !self.deploying
            && !self.git_missing
            && self.ready_repo().is_some()
        {
            self.spawn_refresh(true);
        }

        // No in-app title: the OS window chrome already says it.
        // Header is just context (folder name) plus global actions.
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                match self.ready_repo() {
                    // Ready checkout only: a missing/invalid path shows the
                    // guidance line instead of a nonsense folder name.
                    Some(repo) => {
                        let folder = repo
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| repo.to_string_lossy().to_string());
                        ui.add(
                            egui::Label::new(egui::RichText::new(folder).size(15.0).strong())
                                .truncate(),
                        )
                        .on_hover_text(repo.to_string_lossy());
                    }
                    _ => {
                        let configured = self.config.mod_repo_path.trim();
                        let msg = if configured.is_empty() {
                            "mod checkout not found — Find or Checkout below".to_string()
                        } else if Path::new(configured).is_dir() {
                            format!("not a DowagerMod checkout: {configured}")
                        } else {
                            format!("configured mod path not found: {configured}")
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(msg).color(WARN_AMBER)).truncate(),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    // Refresh lives here (not in Branches): it refreshes
                    // status, branches, and the live-match line together.
                    let busy = self.busy.is_some()
                        || self.deploying
                        || self.ready_repo().is_none()
                        || self.git_missing;
                    if ui
                        .add_enabled(!busy, egui::Button::new("Refresh"))
                        .on_hover_text("Refresh status and branches (Ctrl+R)")
                        .clicked()
                    {
                        self.spawn_refresh(true);
                    }
                    if ui
                        .button("Report bug")
                        .on_hover_text("File a GitHub issue with redacted diagnostics")
                        .clicked()
                    {
                        self.report_result.clear();
                        self.show_report = true;
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("log").show(ctx, |ui| {
            ui.add_space(2.0);
            if let Some(busy) = &self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new(busy).weak());
                });
            }
            if self.deploying {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("installer running…").weak());
                });
            }
            if !self.log.is_empty() {
                let color = if looks_like_error(&self.log) {
                    ERR_RED
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.label(egui::RichText::new(&self.log).small().color(color));
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    if self.git_missing || self.git_installing || !self.git_error.is_empty() {
                        self.git_banner_ui(ui);
                        ui.add_space(8.0);
                        ui.separator();
                    }
                    // Settings replaces the pane (no bump-down); the
                    // selection screen replaces it when no checkout is
                    // ready.
                    if self.show_settings {
                        self.settings_ui(ui);
                    } else if self.ready_repo().is_none() {
                        self.no_repo_ui(ui);
                    } else {
                        self.status_ui(ui);
                        ui.add_space(8.0);
                        ui.separator();
                        self.branches_ui(ui, ctx);
                        ui.add_space(8.0);
                        ui.separator();
                        self.civ_ui(ui);
                    }
                    ui.add_space(6.0);
                });
        });

        if self.show_report {
            // Repaint while sending so the spinner animates; result
            // arrival repaints via the mpsc poll anyway.
            if self.report_sending || matches!(self.gh_flow, GhFlow::Installing | GhFlow::Authing) {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            self.report_ui(ctx);
        }

        if self.update_offer.is_some() {
            if self.update_downloading {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            self.update_ui(ctx);
        }

        if self.show_clone {
            self.clone_ui(ctx);
        }
    }
}

impl LauncherApp {
    /// Non-blocking git installer banner: playing stays available while
    /// git is missing; only the git-backed buttons gate off it.
    fn git_banner_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .corner_radius(egui::CornerRadius::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if self.git_installing {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("installing git via winget… (approve any prompt; takes a few minutes)");
                    });
                } else if !self.git_error.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("git install failed: {}", self.git_error))
                            .color(ERR_RED),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Retry install").clicked() {
                            self.spawn_git_install();
                        }
                        ui.label(
                            egui::RichText::new("or install git yourself and hit Retry")
                                .small()
                                .weak(),
                        );
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "Git is not installed — branch, update, and checkout features need it.",
                            )
                            .color(WARN_AMBER),
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .button("Install git")
                            .on_hover_text("Install Git.Git via winget")
                            .clicked()
                        {
                            self.spawn_git_install();
                        }
                        ui.label(
                            egui::RichText::new("or install it yourself and hit Retry")
                                .small()
                                .weak(),
                        );
                    });
                }
            });
    }

    /// Whole-UI empty state: find the checkout on disk or clone it fresh.
    fn no_repo_ui(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "DowagerMod checkout not found");
        ui.label(
            egui::RichText::new(
                "Point the launcher at your clone, or clone a fresh one from GitHub.",
            )
            .weak(),
        );
        if !self.find_error.is_empty() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&self.find_error).color(ERR_RED));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let picking = self.picking.is_some() || self.busy.is_some();
            if ui
                .add_enabled(!picking, primary_button("Find DowagerMod…", !picking))
                .on_hover_text("Pick the DowagerMod folder on disk")
                .clicked()
            {
                self.find_error.clear();
                self.spawn_pick(PickTarget::FindRepo);
            }
            // Checkout shells to git: gated until git is ready.
            let can_clone = !picking && !self.git_missing && !self.git_installing;
            if ui
                .add_enabled(can_clone, primary_button("Checkout DowagerMod", can_clone))
                .on_hover_text("Clone a fresh copy from GitHub")
                .on_disabled_hover_text("install git first")
                .clicked()
            {
                self.open_clone_dialog();
            }
        });
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("You can also set the path manually in Settings.")
                .small()
                .weak(),
        );
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Installed version");
        match &self.status {
            Some(st) => {
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::same(12))
                    .corner_radius(egui::CornerRadius::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&st.branch).size(17.0).strong());
                            ui.label(egui::RichText::new(&st.short_hash).monospace().weak());
                            if !st.commit_date.is_empty() {
                                ui.label(egui::RichText::new(&st.commit_date).weak());
                            }
                        });
                        if !st.subject.is_empty() {
                            ui.label(&st.subject);
                        }
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(git::distance_text(st))
                                .color(distance_color(st))
                                .strong(),
                        );
                        match st
                            .fetched_at
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        {
                            Some(age) => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "last fetch: {}",
                                        git::relative_time(age.as_secs() as i64, now_unix())
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                            None if st.fetch_error.is_none() => {
                                ui.label(egui::RichText::new("not fetched yet").small().weak());
                            }
                            None => {
                                ui.label(egui::RichText::new("fetch state unknown").small().weak());
                            }
                        }
                        if let Some(err) = &st.fetch_error {
                            ui.label(
                                egui::RichText::new(format!("fetch failed (offline?): {err}"))
                                    .small()
                                    .color(WARN_AMBER),
                            );
                        }
                        // Full path lives here (not the header): always
                        // visible so a wrong checkout is obvious.
                        if let Some(repo) = &self.repo {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Location:").small().weak());
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(repo.to_string_lossy()).monospace(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(repo.to_string_lossy());
                            });
                        }
                    });
            }
            None => {
                if self.repo.is_none() {
                    ui.label(egui::RichText::new("no checkout configured — open Settings").weak());
                } else {
                    ui.label(egui::RichText::new("no status yet").weak());
                }
            }
        }
        // The button only appears when there is something to do: a
        // branch behind its tip. Current, detached, and still-loading
        // states show just the distance line (no dead button).
        if matches!(&self.status, Some(st) if !st.detached && st.behind > 0) {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // Blocked while deploying: the payload must not change
                // mid-install. The git layer re-checks anyway.
                let enabled = self.busy.is_none() && !self.deploying && !self.git_missing;
                if ui
                    .add_enabled(enabled, primary_button("Update to latest", enabled))
                    .on_hover_text("Fast-forward this branch to its remote tip")
                    .clicked()
                {
                    self.spawn_update();
                }
            });
        }
    }

    fn branches_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Branches").size(16.0).strong());
            let total = self.branches.len();
            let shown = self.filtered().len();
            let counts = if self.search.trim().is_empty() {
                format!("{total} branches · newest first")
            } else {
                format!("{shown} of {total} · newest first")
            };
            ui.label(egui::RichText::new(counts).small().weak());
        });
        ui.add_space(4.0);
        let search_id = ui.make_persistent_id("branch-search");
        // Read navigation keys BEFORE the text box runs: a single-line
        // TextEdit surrenders focus on Enter, so checking focus afterwards
        // would miss the keypress entirely.
        let had_focus = ui.memory(|m| m.has_focus(search_id));
        if had_focus {
            let count = self.filtered().len();
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                let (next, moved) = move_selection(self.selected, count, true);
                self.selected = next;
                self.scroll_to_selected = moved;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                let (next, moved) = move_selection(self.selected, count, false);
                self.selected = next;
                self.scroll_to_selected = moved;
            }
        }
        let resp = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .id(search_id)
                .hint_text("type to filter, Up/Down to move, Enter to switch"),
        );
        if !self.focused_search {
            self.focused_search = true;
            resp.request_focus();
        }
        if resp.changed() {
            self.selected = 0;
            self.scroll_to_selected = true;
        }
        // Single-line TextEdit surrenders focus on Enter: lost focus plus
        // the Enter key itself is the submit signal.
        if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.checkout_selected();
            ctx.memory_mut(|m| m.request_focus(search_id));
        }
        let now = now_unix();
        // Owned clone: the row closure calls &mut self methods, so it must
        // not hold an immutable borrow of self via filtered().
        let filtered: Vec<BranchInfo> = self.filtered().into_iter().cloned().collect();
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for (i, b) in filtered.iter().enumerate() {
                    let age = git::relative_time(b.commit_time_unix, now);
                    let resp = ui.selectable_label(i == self.selected, branch_row_job(b, &age, ui));
                    if resp.clicked() {
                        self.selected = i;
                        // Rows aren't focusable: keep keyboard nav alive
                        // by handing focus back to the filter box.
                        ctx.memory_mut(|m| m.request_focus(search_id));
                    }
                    if resp.double_clicked() {
                        self.selected = i;
                        self.checkout_selected();
                    }
                    if i == self.selected && self.scroll_to_selected {
                        // `None` = minimal scroll: only moves when the row
                        // is out of view (centering would yank on every key).
                        resp.scroll_to_me(None);
                    }
                }
                if filtered.is_empty() {
                    ui.label("no branches match the filter");
                }
            });
        self.scroll_to_selected = false;
        ui.horizontal(|ui| {
            let busy =
                self.busy.is_some() || self.deploying || self.repo.is_none() || self.git_missing;
            if ui
                .add_enabled(!busy, egui::Button::new("Switch to selected"))
                .on_hover_text("Check out the selected branch (Enter)")
                .clicked()
            {
                self.checkout_selected();
            }
        });
    }

    fn civ_ui(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Civilization IV: Beyond the Sword");
        let plan = civ::resolve(&self.config.civ_exe_override, self.config.steam_app_id);
        ui.label(
            egui::RichText::new(format!("launch via: {plan}"))
                .monospace()
                .small()
                .weak(),
        );
        if !self.config.civ_exe_override.trim().is_empty()
            && !civ::is_bts_exe_name(&self.config.civ_exe_override)
        {
            ui.label(
                egui::RichText::new(
                    "override doesn't look like the BTS exe (expected Civ4BeyondSword.exe)",
                )
                .color(WARN_AMBER),
            );
        }
        match civ::installed_game_dir() {
            Some(dir) => {
                let deployed = civ::is_mod_deployed(&dir);
                let text = if deployed {
                    "mod is deployed in the live install"
                } else {
                    "live install found but mod sentinel missing — run Deploy"
                };
                ui.label(egui::RichText::new(text).color(if deployed {
                    OK_GREEN
                } else {
                    WARN_AMBER
                }));
                if deployed {
                    match self.live_match {
                        civ::LiveMatch::Matches => {
                            ui.label(
                                egui::RichText::new("live install matches this checkout")
                                    .color(OK_GREEN),
                            );
                        }
                        civ::LiveMatch::Differs => {
                            ui.label(
                                egui::RichText::new(
                                    "live install differs from this checkout — run Deploy",
                                )
                                .color(WARN_AMBER),
                            );
                        }
                        civ::LiveMatch::Unknown => {}
                    }
                }
                if let Some(v) = civ::last_deployed_version() {
                    ui.label(
                        egui::RichText::new(format!("last deployed mod version: {v}"))
                            .small()
                            .weak(),
                    );
                }
                if let Some(at) = civ::last_deployed_at() {
                    ui.label(
                        egui::RichText::new(format!("last deployed: {}", at.replace('T', " ")))
                            .small()
                            .weak(),
                    );
                }
            }
            None => {
                ui.label(
                    egui::RichText::new("live game install not found (installer has not run yet?)")
                        .color(WARN_AMBER),
                );
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // Launch stays enabled without a repo: the Steam fallback
            // needs no checkout. Blocked while deploying: launching
            // mid-wipe would start a half-mirrored game.
            let can_launch = !self.deploying;
            if ui
                .add_enabled(
                    can_launch,
                    primary_button("Launch Civ4 with DowagerMod", can_launch),
                )
                .on_hover_text("Start Civilization IV with the deployed mod")
                .on_disabled_hover_text("installer is running — close its window first")
                .clicked()
            {
                match civ::launch(&plan) {
                    Ok(msg) => self.set_log(msg),
                    Err(e) => self.set_log(e),
                }
            }
            let busy = self.busy.is_some() || self.deploying || self.repo.is_none();
            if ui
                .add_enabled(!busy, egui::Button::new("Deploy to Civ4"))
                .on_hover_text("Run Install DowagerMod.bat (UAC prompt appears)")
                .clicked()
            {
                self.spawn_deploy();
            }
        });
        ui.label(
            egui::RichText::new(
                "Deploy runs Install DowagerMod.bat (UAC prompt appears). Launch after deploying.",
            )
            .small()
            .weak(),
        );
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Settings");
        let detected = config::default_repo_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "auto-detect: not found".to_string());
        // Manual rows (not a Grid): grids size columns to content and
        // never expand the TextEdits, leaving unusably narrow fields.
        let label = |ui: &mut egui::Ui, text: &str| {
            ui.add_sized([118.0, 20.0], egui::Label::new(text));
        };
        ui.horizontal(|ui| {
            label(ui, "Mod repo path:");
            // Button first from the right: a full-width field would
            // otherwise shove it off-screen.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(self.picking.is_none(), egui::Button::new("Browse…"))
                    .on_hover_text("Pick the DowagerMod checkout folder")
                    .clicked()
                {
                    self.spawn_pick(PickTarget::RepoDir);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft_repo_path)
                        .desired_width(f32::INFINITY)
                        .hint_text(format!("empty = auto-detect ({detected})")),
                );
            });
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            label(ui, "Steam app id:");
            ui.add(egui::TextEdit::singleline(&mut self.draft_app_id).desired_width(f32::INFINITY));
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            label(ui, "Civ exe override:");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(self.picking.is_none(), egui::Button::new("Browse…"))
                    .on_hover_text("Pick Civ4BeyondSword.exe")
                    .clicked()
                {
                    self.spawn_pick(PickTarget::CivExe);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft_exe).desired_width(f32::INFINITY),
                );
            });
        });
        ui.add_space(4.0);
        // Gated while any job runs: a repo switch must not overlap git
        // (lock fights) or the installer (payload reads). Bounded by the
        // 90s git timeout at worst.
        let save_idle = self.busy.is_none() && !self.deploying;
        if ui
            .add_enabled(save_idle, egui::Button::new("Save"))
            .clicked()
        {
            // Validate before touching anything: an invalid path refuses
            // the save instead of wedging the UI into a broken state.
            // (Empty repo path = auto-detect, always allowed.)
            let repo_draft = self.draft_repo_path.trim().to_string();
            if !repo_draft.is_empty() {
                if let Err(e) = git::validate_repo(Path::new(&repo_draft)) {
                    self.set_log(e);
                    return;
                }
            }
            if let Err(e) = civ::validate_exe_override(&self.draft_exe) {
                self.set_log(e);
                return;
            }
            self.config.mod_repo_path = repo_draft;
            self.config.steam_app_id =
                config::parse_app_id(&self.draft_app_id, self.config.steam_app_id);
            self.config.civ_exe_override = self.draft_exe.trim().to_string();
            match self.config.save() {
                Ok(()) => self.set_log("settings saved".to_string()),
                Err(e) => self.set_log(format!("save failed: {e}")),
            }
            self.repo = self.config.repo_path();
            // Orphan anything still in flight (defensive: Save is gated,
            // but a result could already be queued) and never show the
            // previous repo's status under a new path.
            self.job_epoch += 1;
            self.status = None;
            self.branches.clear();
            self.selected = 0;
            self.show_settings = self.repo.is_none();
            if self.repo.is_none() {
                // Nothing will complete: don't wedge `busy` on.
                self.busy = None;
            }
            self.spawn_refresh(true);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .button("Open log file")
                .on_hover_text("Open launcher.log in the default text viewer")
                .clicked()
            {
                match crate::log::path() {
                    Some(path) => {
                        if let Err(e) = open::that(&path) {
                            self.set_log(format!("failed to open log: {e}"));
                        }
                    }
                    None => self.set_log("no log dir on this machine".to_string()),
                }
            }
            ui.label(
                egui::RichText::new("session log with every git command + result")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let checking = self.update_checking || self.update_downloading;
            if ui
                .add_enabled(!checking, egui::Button::new("Check for updates"))
                .on_hover_text("Check GitHub Releases for a newer launcher build")
                .clicked()
            {
                self.spawn_update_check();
            }
            ui.label(
                egui::RichText::new(update_row_text(
                    &update::local_version(),
                    &self.update_status,
                ))
                .small()
                .weak(),
            );
        });
        ui.add_space(8.0);
        // Way back: Settings replaces the whole pane.
        if ui.button("Close").clicked() {
            self.show_settings = false;
        }
    }

    fn report_ui(&mut self, ctx: &egui::Context) {
        // Local copy: `Window::open` needs `&mut bool`, which can't
        // borrow the field while the contents closure borrows `self`.
        let mut open = self.show_report;
        egui::Window::new("Report bug")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| {
                ui.label("Title:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.report_title)
                        .desired_width(f32::INFINITY)
                        .hint_text("crash when …"),
                );
                ui.add_space(4.0);
                ui.label("What happened:");
                ui.add(
                    egui::TextEdit::multiline(&mut self.report_desc)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .hint_text("steps, what you expected, what you saw"),
                );
                ui.label(
                    egui::RichText::new(
                        "Diagnostics (mod version, installer config, launcher + Civ4 log tails) \
                         attach automatically with paths, host, and IPs redacted.",
                    )
                    .small()
                    .weak(),
                );
                if let Some(preview) = &self.report_preview {
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(preview).monospace().small());
                        });
                }
                if !self.report_result.is_empty() {
                    ui.add_space(4.0);
                    if let Some(url) = self
                        .report_result
                        .strip_prefix("issue filed: ")
                        .map(str::trim)
                    {
                        ui.horizontal(|ui| {
                            ui.label("issue filed:");
                            ui.hyperlink(url);
                        });
                    } else {
                        let color = if looks_like_error(&self.report_result) {
                            ERR_RED
                        } else {
                            OK_GREEN
                        };
                        ui.label(
                            egui::RichText::new(&self.report_result)
                                .small()
                                .color(color),
                        );
                    }
                }
                // Guided gh setup: only appears once a submit discovers
                // gh missing or unauthenticated.
                match self.gh_flow {
                    GhFlow::Idle => {}
                    GhFlow::Missing => {
                        ui.add_space(4.0);
                        ui.label("GitHub CLI (gh) is not installed — it files the issue.");
                        if !self.gh_message.is_empty() {
                            ui.label(egui::RichText::new(&self.gh_message).small().color(ERR_RED));
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .button("Install gh")
                                .on_hover_text("Install GitHub.cli via winget")
                                .clicked()
                            {
                                self.spawn_gh_install();
                            }
                            if ui
                                .button("Just open in browser")
                                .on_hover_text("Skip gh: prefilled issue form instead")
                                .clicked()
                            {
                                let body = self.build_report_body();
                                let url =
                                    report::issue_prefill_url(self.report_title.trim(), &body);
                                match open::that(&url) {
                                    Ok(()) => {
                                        self.gh_flow = GhFlow::Idle;
                                        self.report_result =
                                            "opened prefilled issue in browser".to_string();
                                    }
                                    Err(e) => {
                                        self.gh_message = format!("could not open browser: {e}");
                                    }
                                }
                            }
                        });
                    }
                    GhFlow::Installing => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(
                                    "installing gh via winget… (approve any prompt)",
                                )
                                .weak(),
                            );
                        });
                    }
                    GhFlow::NeedsAuth => {
                        ui.add_space(4.0);
                        ui.label("gh is installed but not signed in (one-time).");
                        if !self.gh_message.is_empty() {
                            ui.label(egui::RichText::new(&self.gh_message).small().color(ERR_RED));
                        }
                        ui.label(
                            egui::RichText::new(
                                "1. Click Authenticate — a terminal opens.\n\
                                 2. Choose GitHub.com and log in with your browser.\n\
                                 3. Finish there; submitting continues here automatically.",
                            )
                            .small()
                            .weak(),
                        );
                        if ui.button("Authenticate…").clicked() {
                            self.spawn_gh_auth();
                        }
                    }
                    GhFlow::Authing => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("waiting for the sign-in terminal…").weak(),
                            );
                        });
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let sending = self.report_sending;
                    let preview_label = if self.report_preview.is_some() {
                        "Hide preview"
                    } else {
                        "Preview diagnostics"
                    };
                    if ui
                        .add_enabled(!sending, egui::Button::new(preview_label))
                        .on_hover_text("Review exactly what gets attached")
                        .clicked()
                    {
                        if self.report_preview.is_some() {
                            self.report_preview = None;
                        } else {
                            self.report_preview = Some(self.build_report_body());
                        }
                    }
                    if ui
                        .button("Copy diagnostics")
                        .on_hover_text("Copy the full report body to the clipboard")
                        .clicked()
                    {
                        ctx.copy_text(self.build_report_body());
                        self.report_result = "diagnostics copied to clipboard".to_string();
                    }
                    let gh_busy = matches!(self.gh_flow, GhFlow::Installing | GhFlow::Authing);
                    let can_submit = !sending && !gh_busy && !self.report_title.trim().is_empty();
                    if ui
                        .add_enabled(can_submit, primary_button("Submit issue", can_submit))
                        .on_hover_text(
                            "File on siegelh/DowagerMod via gh (installs and signs in when needed)",
                        )
                        .clicked()
                    {
                        let title = self.report_title.trim().to_string();
                        self.submit_or_setup_gh(title, self.report_desc.clone());
                    }
                    if sending {
                        ui.spinner();
                        ui.label(egui::RichText::new("filing issue…").weak());
                    }
                });
            });
        self.show_report = open;
    }

    fn update_ui(&mut self, ctx: &egui::Context) {
        let Some(offer) = self.update_offer.clone() else {
            return;
        };
        // Closing the window is "Later" (same as the button below).
        let mut open = true;
        egui::Window::new("Update available")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} is available (you have v{}).",
                    offer.tag,
                    update::local_version()
                ));
                ui.label(
                    egui::RichText::new("Update downloads the new build and restarts.")
                        .small()
                        .weak(),
                );
                if !self.update_error.is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(&self.update_error)
                            .small()
                            .color(ERR_RED),
                    );
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Or get it here:").small().weak());
                        ui.hyperlink(update::release_page_url(&offer.tag));
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let downloading = self.update_downloading;
                    let can_update = !downloading;
                    if ui
                        .add_enabled(can_update, primary_button("Update & restart", can_update))
                        .on_hover_text("Download the new build and restart the launcher")
                        .clicked()
                    {
                        self.spawn_update_download(offer.clone());
                    }
                    if ui
                        .add_enabled(!downloading, egui::Button::new("Later"))
                        .clicked()
                    {
                        self.update_offer = None;
                        self.update_error.clear();
                    }
                    if downloading {
                        ui.spinner();
                        ui.label(egui::RichText::new("downloading…").weak());
                    }
                });
            });
        if !open {
            self.update_offer = None;
            self.update_error.clear();
        }
    }

    fn clone_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.show_clone;
        egui::Window::new("Checkout DowagerMod")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label("Destination:");
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.picking.is_none(), egui::Button::new("Browse…"))
                            .on_hover_text("Pick an empty folder to clone into")
                            .clicked()
                        {
                            self.spawn_pick(PickTarget::CloneDest);
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.clone_dest)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
                ui.label(
                    egui::RichText::new(format!(
                        "Clones {} (a first checkout downloads the whole history).",
                        git::MOD_REPO_URL,
                    ))
                    .small()
                    .weak(),
                );
                if !self.clone_error.is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("clone failed: {}", self.clone_error))
                            .small()
                            .color(ERR_RED),
                    );
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    // In the empty state only the clone itself sets `busy`.
                    let busy = self.busy.is_some() || self.deploying || self.git_missing;
                    let can_clone = !busy && !self.clone_dest.trim().is_empty();
                    if ui
                        .add_enabled(can_clone, primary_button("Clone", can_clone))
                        .clicked()
                    {
                        self.spawn_clone(self.clone_dest.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_clone = false;
                    }
                    if self.busy.is_some() {
                        ui.spinner();
                        ui.label(egui::RichText::new("cloning…").weak());
                    }
                });
            });
        // A running clone owns `busy`, not the dialog: closing mid-clone
        // leaves it running, and the result still applies on completion.
        // AND, not assign: Cancel clears the field inside, and the X
        // clears `open` — either one closes the dialog.
        self.show_clone = open && self.show_clone;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_color_marks_sync_state() {
        let synced = RepoStatus {
            ..Default::default()
        };
        assert_eq!(distance_color(&synced), OK_GREEN);
        let ahead_only = RepoStatus {
            ahead: 3,
            ..Default::default()
        };
        assert_eq!(distance_color(&ahead_only), OK_GREEN);
        let behind = RepoStatus {
            behind: 2,
            ..Default::default()
        };
        assert_eq!(distance_color(&behind), WARN_AMBER);
        let detached = RepoStatus {
            detached: true,
            ..Default::default()
        };
        assert_eq!(distance_color(&detached), ERR_RED);
    }

    #[test]
    fn selection_moves_clamp_at_both_ends() {
        assert_eq!(move_selection(0, 5, true), (1, true));
        assert_eq!(move_selection(1, 5, false), (0, true));
        assert_eq!(move_selection(4, 5, true), (4, false));
        assert_eq!(move_selection(0, 5, false), (0, false));
        assert_eq!(move_selection(0, 0, true), (0, false));
        assert_eq!(move_selection(9, 5, true), (4, false));
        assert_eq!(move_selection(9, 5, false), (3, true));
    }

    #[test]
    fn checkout_skips_current_branch() {
        assert!(!needs_checkout(Some("main"), "main"));
        assert!(needs_checkout(Some("main"), "frost"));
        assert!(needs_checkout(None, "frost"));
    }

    #[test]
    fn update_row_always_lists_version() {
        assert_eq!(update_row_text("0.2.1", ""), "launcher v0.2.1");
        assert_eq!(
            update_row_text("0.2.1", "up to date"),
            "launcher v0.2.1 · up to date"
        );
    }

    #[test]
    fn error_sniffing_flags_failures_only() {
        assert!(looks_like_error("fetch failed (offline?): nope"));
        assert!(looks_like_error("update failed: Permission denied"));
        assert!(looks_like_error("install failed: winget is missing"));
        assert!(looks_like_error("clone failed: destination already exists"));
        assert!(looks_like_error(
            "not a DowagerMod checkout (installer not found): C:\\x"
        ));
        assert!(looks_like_error("git fetch timed out after 90s"));
        assert!(looks_like_error(
            "deploy refused: Civ4 is running — quit the game first"
        ));
        assert!(looks_like_error("not a git checkout: C:\\nope"));
        assert!(looks_like_error("error: cannot open '.git/FETCH_HEAD'"));
        assert!(looks_like_error(
            "installer already running in another window — wait for it to finish"
        ));
        assert!(!looks_like_error("already up to date"));
        assert!(!looks_like_error("branch switched"));
        assert!(!looks_like_error("settings saved"));
        assert!(!looks_like_error("installer window closed"));
        assert!(!looks_like_error(""));
        // Success output naming an `error_*` file must stay unflagged.
        assert!(!looks_like_error(
            "Updating abc..def\nFast-forward\n error_handler.py | 2 +-"
        ));
    }

    #[test]
    fn browse_start_dir_prefers_existing_dir_then_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // The draft itself when it exists.
        assert_eq!(pick_start_dir(&root.to_string_lossy()), Some(root.clone()));
        // Its parent when the draft is a file or a missing path.
        let missing = root.join("nope").join("deeper");
        assert_eq!(
            pick_start_dir(&missing.to_string_lossy()),
            None,
            "missing parents fall through to the dialog default"
        );
        assert_eq!(
            pick_start_dir(&root.join("file.exe").to_string_lossy()),
            Some(root.clone())
        );
        assert_eq!(pick_start_dir(""), None);
        assert_eq!(pick_start_dir("   "), None);
    }
}
