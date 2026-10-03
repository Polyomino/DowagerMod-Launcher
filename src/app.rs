//! eframe/egui front end: status card, searchable branch list, update/deploy/launch.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::LauncherConfig;
use crate::git::{self, BranchInfo, RepoStatus};
use crate::{civ, config};

/// Message from a background worker thread to the UI thread.
enum JobOut {
    /// Full refresh finished: refresh epoch plus status + branches, or an
    /// error string. Stale epochs (a newer refresh is in flight) are dropped.
    Refreshed(u64, Result<(RepoStatus, Vec<BranchInfo>), String>),
    /// `git pull --ff-only` finished.
    Updated(Result<String, String>),
    /// `git checkout` finished.
    CheckedOut(Result<String, String>),
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
    refresh_epoch: u64,
    tx: Sender<JobOut>,
    rx: Receiver<JobOut>,
}

impl LauncherApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
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
            refresh_epoch: 0,
            tx,
            rx,
        };
        // Fast first paint: synchronous local-only snapshot (no network
        // fetch), then a background thread does fetch + full refresh.
        if let Some(repo) = app.repo.clone() {
            match git::status(&repo, false) {
                Ok(st) => app.status = Some(st),
                Err(e) => app.log = e,
            }
            if let Ok(list) = git::branches(&repo) {
                app.branches = list;
            }
            app.spawn_refresh(true);
        } else {
            app.log = "DowagerMod checkout not found — set the path in Settings.".to_string();
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
        self.refresh_epoch += 1;
        let epoch = self.refresh_epoch;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = git::status(&repo, fetch_first)
                .and_then(|st| git::branches(&repo).map(|list| (st, list)));
            let _ = tx.send(JobOut::Refreshed(epoch, out));
        });
    }

    fn spawn_update(&mut self) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.busy = Some("updating…".to_string());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(JobOut::Updated(git::update_to_latest(&repo)));
        });
    }

    fn spawn_checkout(&mut self, branch: String) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.busy = Some(format!("switching to {branch}…"));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(JobOut::CheckedOut(git::checkout(&repo, &branch)));
        });
    }

    fn poll_jobs(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                JobOut::Refreshed(epoch, Ok((mut st, list))) => {
                    if epoch != self.refresh_epoch {
                        continue; // stale: a newer refresh is in flight
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
                    self.selected = keep
                        .and_then(|name| self.filtered().iter().position(|b| b.name == name))
                        .unwrap_or(0);
                    self.busy = None;
                }
                JobOut::Refreshed(epoch, Err(e)) => {
                    if epoch != self.refresh_epoch {
                        continue; // stale: a newer refresh is in flight
                    }
                    self.log = format!("refresh failed: {e}");
                    self.busy = None;
                }
                JobOut::Updated(Ok(out)) => {
                    self.log = if out.is_empty() {
                        "already up to date".to_string()
                    } else {
                        out
                    };
                    self.spawn_refresh(false);
                }
                JobOut::Updated(Err(e)) => {
                    self.log = format!("update failed: {e}");
                    self.busy = None;
                }
                JobOut::CheckedOut(Ok(_)) => {
                    self.log = "branch switched".to_string();
                    self.spawn_refresh(false);
                }
                JobOut::CheckedOut(Err(e)) => {
                    self.log = format!("switch failed: {e}");
                    self.busy = None;
                }
            }
        }
    }

    fn filtered(&self) -> Vec<&BranchInfo> {
        git::filter_branches(&self.branches, &self.search)
    }

    fn checkout_selected(&mut self) {
        // Enter/double-click bypass the disabled-button gate, so guard here:
        // overlapping git jobs fight over the repo lock.
        if self.busy.is_some() {
            return;
        }
        if let Some(b) = self.filtered().get(self.selected).map(|b| b.name.clone()) {
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
        if self.busy.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("DowagerMod Launcher");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                });
            });
            match &self.repo {
                Some(repo) => {
                    ui.label(format!("mod: {}", repo.to_string_lossy()));
                }
                None => {
                    let configured = self.config.mod_repo_path.trim();
                    let msg = if configured.is_empty() {
                        "mod checkout not found — set the path in Settings".to_string()
                    } else {
                        format!("configured mod path not found: {configured}")
                    };
                    ui.colored_label(egui::Color32::YELLOW, msg);
                }
            }
        });

        egui::TopBottomPanel::bottom("log").show(ctx, |ui| {
            if let Some(busy) = &self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(busy);
                });
            }
            if !self.log.is_empty() {
                ui.label(&self.log);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if self.show_settings {
                        self.settings_ui(ui);
                        ui.separator();
                    }
                    self.status_ui(ui);
                    ui.separator();
                    self.branches_ui(ui, ctx);
                    ui.separator();
                    self.civ_ui(ui);
                });
        });
    }
}

impl LauncherApp {
    fn status_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Installed version");
        match &self.status {
            Some(st) => {
                ui.horizontal(|ui| {
                    ui.label("branch:");
                    ui.monospace(&st.branch);
                });
                ui.horizontal(|ui| {
                    ui.label("commit:");
                    ui.monospace(&st.short_hash);
                    ui.label(&st.subject);
                });
                if !st.commit_date.is_empty() {
                    ui.label(format!("date: {}", st.commit_date));
                }
                ui.label(git::distance_text(st));
                match st
                    .fetched_at
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                {
                    Some(age) => {
                        ui.label(format!(
                            "last fetch: {}",
                            git::relative_time(age.as_secs() as i64, now_unix())
                        ));
                    }
                    None if st.fetch_error.is_none() => {
                        ui.label("not fetched yet");
                    }
                    None => {
                        ui.label("fetch state unknown");
                    }
                }
                if let Some(err) = &st.fetch_error {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!("fetch failed (offline?): {err}"),
                    );
                }
            }
            None => {
                ui.label("no status yet");
            }
        }
        ui.horizontal(|ui| {
            // No status (or a detached HEAD) means there is nothing sane to
            // fast-forward; the git layer re-checks anyway.
            let can_update =
                self.repo.is_some() && matches!(&self.status, Some(st) if !st.detached);
            if ui
                .add_enabled(
                    self.busy.is_none() && can_update,
                    egui::Button::new("Update to latest"),
                )
                .clicked()
            {
                self.spawn_update();
            }
        });
    }

    fn branches_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Branches (newest first)");
        let search_id = ui.make_persistent_id("branch-search");
        // Read navigation keys BEFORE the text box runs: a single-line
        // TextEdit surrenders focus on Enter, so checking focus afterwards
        // would miss the keypress entirely.
        let had_focus = ui.memory(|m| m.has_focus(search_id));
        if had_focus {
            let count = self.filtered().len();
            if count > 0
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
            {
                self.selected = (self.selected + 1).min(count - 1);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.selected = self.selected.saturating_sub(1);
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
                    let label = format!(
                        "{}  {}  {}  {}{}",
                        b.name,
                        b.short_hash,
                        git::relative_time(b.commit_time_unix, now),
                        b.subject,
                        if b.remote_only { "  [remote]" } else { "" },
                    );
                    let mut text = egui::RichText::new(label);
                    if b.is_current {
                        text = text.strong();
                    }
                    let resp = ui.selectable_label(i == self.selected, text);
                    if resp.clicked() {
                        self.selected = i;
                    }
                    if resp.double_clicked() {
                        self.selected = i;
                        self.checkout_selected();
                    }
                    if i == self.selected {
                        resp.scroll_to_me(Some(egui::Align::Center));
                    }
                }
                if filtered.is_empty() {
                    ui.label("no branches match the filter");
                }
            });
        ui.horizontal(|ui| {
            let busy = self.busy.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new("Switch to selected"))
                .clicked()
            {
                self.checkout_selected();
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Refresh"))
                .clicked()
            {
                self.spawn_refresh(true);
            }
        });
    }

    fn civ_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Civilization IV: Beyond the Sword");
        let plan = civ::resolve(&self.config.civ_exe_override, self.config.steam_app_id);
        ui.label(format!("launch via: {plan}"));
        match civ::installed_game_dir() {
            Some(dir) => {
                let deployed = civ::is_mod_deployed(&dir);
                let text = if deployed {
                    "mod is deployed in the live install"
                } else {
                    "live install found but mod sentinel missing — run Deploy"
                };
                ui.label(text);
                if let Some(v) = civ::last_deployed_version() {
                    ui.label(format!("last deployed mod version: {v}"));
                }
            }
            None => {
                ui.label("live game install not found (installer has not run yet?)");
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Launch Civ4 with DowagerMod").clicked() {
                match civ::launch(&plan) {
                    Ok(msg) => self.log = msg,
                    Err(e) => self.log = e,
                }
            }
            let busy = self.busy.is_some() || self.repo.is_none();
            if ui
                .add_enabled(!busy, egui::Button::new("Deploy to Civ4"))
                .clicked()
            {
                match self.repo.clone() {
                    Some(repo) => match civ::deploy(&repo) {
                        Ok(msg) => self.log = msg,
                        Err(e) => self.log = e,
                    },
                    None => self.log = "no mod checkout configured".to_string(),
                }
            }
        });
        ui.label(
            "Deploy runs Install DowagerMod.bat (UAC prompt appears). Launch after deploying.",
        );
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.horizontal(|ui| {
            ui.label("mod repo path:");
            let detected = config::default_repo_path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "auto-detect: not found".to_string());
            ui.add(
                egui::TextEdit::singleline(&mut self.draft_repo_path)
                    .hint_text(format!("empty = auto-detect ({detected})")),
            );
        });
        ui.horizontal(|ui| {
            ui.label("steam app id:");
            ui.text_edit_singleline(&mut self.draft_app_id);
        });
        ui.horizontal(|ui| {
            ui.label("civ exe override:");
            ui.text_edit_singleline(&mut self.draft_exe);
        });
        if ui.button("Save").clicked() {
            self.config.mod_repo_path = self.draft_repo_path.trim().to_string();
            self.config.steam_app_id =
                config::parse_app_id(&self.draft_app_id, self.config.steam_app_id);
            self.config.civ_exe_override = self.draft_exe.trim().to_string();
            match self.config.save() {
                Ok(()) => self.log = "settings saved".to_string(),
                Err(e) => self.log = format!("save failed: {e}"),
            }
            self.repo = self.config.repo_path();
            // Never show the previous repo's status under a new path.
            self.status = None;
            self.branches.clear();
            self.selected = 0;
            self.show_settings = self.repo.is_none();
            self.spawn_refresh(true);
        }
    }
}
