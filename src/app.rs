//! eframe/egui front end: status card, searchable branch list, update/deploy/launch.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::LauncherConfig;
use crate::git::{self, BranchInfo, RepoStatus};
use crate::{civ, config};

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
    tx: Sender<JobOut>,
    rx: Receiver<JobOut>,
}

impl LauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_style(&cc.egui_ctx);
        let config = LauncherConfig::load();
        let repo = config.repo_path();
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
            tx,
            rx,
        };
        // Fast first paint: synchronous local-only snapshot (no network
        // fetch), then a background thread does fetch + full refresh.
        if let Some(repo) = app.repo.clone() {
            match git::status(&repo, false) {
                Ok(st) => app.status = Some(st),
                Err(e) => app.set_log(e),
            }
            if let Ok(list) = git::branches(&repo) {
                app.branches = list;
            }
            app.spawn_refresh(true);
        } else {
            app.set_log("DowagerMod checkout not found — set the path in Settings.".to_string());
            app.show_settings = true;
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

    fn checkout_selected(&mut self) {
        // Enter/double-click bypass the disabled-button gate, so guard here:
        // overlapping git jobs fight over the repo lock, and the payload
        // must not change while the installer reads it.
        if self.busy.is_some() || self.deploying {
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
            && self.repo.is_some()
        {
            self.spawn_refresh(true);
        }

        // No in-app title: the OS window chrome already says it.
        // Header is just context (folder name) plus global actions.
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                match &self.repo {
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
                    None => {
                        let configured = self.config.mod_repo_path.trim();
                        let msg = if configured.is_empty() {
                            "mod checkout not found — set the path in Settings".to_string()
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
                    let busy = self.busy.is_some() || self.deploying || self.repo.is_none();
                    if ui
                        .add_enabled(!busy, egui::Button::new("Refresh"))
                        .on_hover_text("Refresh status and branches (Ctrl+R)")
                        .clicked()
                    {
                        self.spawn_refresh(true);
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
                    if self.show_settings {
                        self.settings_ui(ui);
                        ui.add_space(8.0);
                        ui.separator();
                    }
                    self.status_ui(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    self.branches_ui(ui, ctx);
                    ui.add_space(8.0);
                    ui.separator();
                    self.civ_ui(ui);
                    ui.add_space(6.0);
                });
        });
    }
}

impl LauncherApp {
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
                        // Full path lives here (not the header): visible
                        // when debugging a wrong repo, quiet otherwise.
                        if let Some(repo) = &self.repo {
                            ui.label(
                                egui::RichText::new(format!("repo: {}", repo.display()))
                                    .small()
                                    .weak(),
                            );
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
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            // No status (or a detached HEAD) means there is nothing sane to
            // fast-forward; the git layer re-checks anyway. Blocked while
            // deploying: the payload must not change mid-install.
            let can_update =
                self.repo.is_some() && matches!(&self.status, Some(st) if !st.detached);
            let enabled = self.busy.is_none() && !self.deploying && can_update;
            if ui
                .add_enabled(enabled, primary_button("Update to latest", enabled))
                .on_hover_text("Fast-forward this branch to its remote tip")
                .clicked()
            {
                self.spawn_update();
            }
        });
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
            let busy = self.busy.is_some() || self.deploying || self.repo.is_none();
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
        egui::Grid::new("settings-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Mod repo path:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft_repo_path)
                        .desired_width(f32::INFINITY)
                        .hint_text(format!("empty = auto-detect ({detected})")),
                );
                ui.end_row();
                ui.label("Steam app id:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft_app_id).desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Civ exe override:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft_exe).desired_width(f32::INFINITY),
                );
                ui.end_row();
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
            self.config.mod_repo_path = self.draft_repo_path.trim().to_string();
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
    fn error_sniffing_flags_failures_only() {
        assert!(looks_like_error("fetch failed (offline?): nope"));
        assert!(looks_like_error("update failed: Permission denied"));
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
}
