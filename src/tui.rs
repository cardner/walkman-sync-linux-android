//! Ratatui dashboard — port of the Python front-end.

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal};

use crate::art::{self, ArtRaster};
use crate::audiomuse;
use crate::catalog;
use crate::likes;
use crate::paths;
use crate::storage;
use crate::sync::{self, AppConfig, ScanReport};
use crate::walkman;

const AMBER: Color = Color::Rgb(255, 176, 32);
const DEST_SOLID: Color = Color::Green;
const DEST_GHOST: Color = Color::Rgb(48, 120, 64);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Dashboard,
    Settings,
    Text,
    Likes,
    LastFm,
    Prompt,
    Confirm,
    Loading,
}

struct App {
    config: AppConfig,
    report: Option<ScanReport>,
    toast: String,
    screen: Screen,
    menu: usize,
    scroll: usize,
    text_title: String,
    text_rows: Vec<String>,
    settings_idx: usize,
    overlay_parent: Screen,
    prompt_label: String,
    prompt_buf: String,
    prompt_kind: String,
    prompt_cursor: usize,
    confirm_q: String,
    confirm_buf: String,
    confirm_action: ConfirmAction,
    likes: Option<likes::Session>,
    likes_plan: Option<likes::Plan>,
    likes_idx: usize,
    progress: Arc<Mutex<Progress>>,
    busy: bool,
    should_quit: bool,
    task: Option<JoinHandle<TaskResult>>,
    scrobble_note: String,
    scrobble_pending: usize,
    copy_jobs: usize,
    art: Mutex<ArtCache>,
}

#[derive(Default)]
struct ArtCache {
    key: Option<(String, u16, u16)>,
    raster: Option<ArtRaster>,
}

enum TaskResult {
    Scan {
        config: AppConfig,
        report: ScanReport,
    },
    Logs {
        title: String,
        logs: Vec<String>,
        rescan: bool,
    },
    SyncPreview {
        logs: Vec<String>,
        config: AppConfig,
        report: ScanReport,
    },
    CleanupPreview {
        logs: Vec<String>,
        config: AppConfig,
        report: ScanReport,
    },
    AudioMuse(audiomuse::PullResult),
    LikesReady {
        session: likes::Session,
        plan: likes::Plan,
    },
    LikesLogs {
        session: likes::Session,
        plan: likes::Plan,
        title: String,
        logs: Vec<String>,
    },
    SignedIn {
        session: likes::Session,
        name: String,
    },
    SignedInFailed {
        session: likes::Session,
        error: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    None,
    Sync,
    Cleanup,
    Scrobbles,
    AudioMuse,
    LikesApply,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProgressUnit {
    #[default]
    Count,
    Bytes,
}

#[derive(Default, Clone)]
struct Progress {
    title: String,
    message: String,
    lines: Vec<String>,
    done: u64,
    total: u64,
    unit: ProgressUnit,
    started: Option<Instant>,
    last_at: Option<Instant>,
    last_bytes: u64,
    bps: Option<f64>,
    ticks: u32,
    eta_secs: Option<f64>,
    eta_at: Option<Instant>,
    dest_total: u64,
    dest_projected: u64,
    dest_freed: u64,
    probe_used: u64,
    probe_done: u64,
    probe_at: Option<Instant>,
}

const ACTIONS: &[(&str, &str, &str)] = &[
    ("1", "Sync music to the Walkman", "sync"),
    ("2", "Plan (added / modified / removed)", "plan"),
    ("r", "Refresh plan", "refresh"),
    ("3", "Playlists", "playlists"),
    ("4", "Duplicates on the Walkman", "duplicates"),
    ("5", "Likes: device ⇄ Last.fm ⇄ MusicBee", "likes"),
    ("6", "Scrobbles: upload to Last.fm", "scrobbles"),
    ("a", "AudioMuse: mixes + scores", "audiomuse"),
    ("9", "Clean up stale files", "cleanup"),
    ("i", "Index library", "index_library"),
    ("w", "Index Walkman", "index_walkman"),
    ("s", "Settings", "settings"),
    ("l", "Last.fm account", "lastfm"),
    ("q", "Quit", "quit"),
];

impl App {
    fn new() -> Self {
        let config = AppConfig::load_without_detect();
        Self {
            config,
            report: None,
            toast: String::new(),
            screen: Screen::Dashboard,
            menu: 0,
            scroll: 0,
            text_title: String::new(),
            text_rows: Vec::new(),
            settings_idx: 0,
            overlay_parent: Screen::Dashboard,
            prompt_label: String::new(),
            prompt_buf: String::new(),
            prompt_kind: String::new(),
            prompt_cursor: 0,
            confirm_q: String::new(),
            confirm_buf: String::new(),
            confirm_action: ConfirmAction::None,
            likes: None,
            likes_plan: None,
            likes_idx: 0,
            progress: Arc::new(Mutex::new(Progress::default())),
            busy: false,
            should_quit: false,
            task: None,
            scrobble_note: "open Scrobbles (8) or rescan".into(),
            scrobble_pending: 0,
            copy_jobs: 0,
            art: Mutex::new(ArtCache::default()),
        }
    }

    fn show_text(&mut self, title: &str, rows: Vec<String>) {
        self.text_title = title.into();
        self.text_rows = cap_text_rows(rows);
        self.scroll = 0;
        self.screen = Screen::Text;
    }

    fn remember_parent(&mut self) {
        if !matches!(
            self.screen,
            Screen::Loading | Screen::Prompt | Screen::Confirm
        ) {
            self.overlay_parent = self.screen;
        }
    }

    fn overlay_back(&self) -> Screen {
        match self.overlay_parent {
            Screen::Loading | Screen::Prompt | Screen::Confirm => Screen::Dashboard,
            other => other,
        }
    }

    fn ask(&mut self, question: &str, action: ConfirmAction) {
        self.remember_parent();
        self.confirm_q = question.into();
        self.confirm_buf.clear();
        self.confirm_action = action;
        self.screen = Screen::Confirm;
    }

    fn prompt(&mut self, label: &str, kind: &str) {
        self.remember_parent();
        self.prompt_label = label.into();
        self.prompt_kind = kind.into();
        self.prompt_buf = prompt_seed(self, kind);
        self.prompt_cursor = self.prompt_buf.chars().count();
        self.screen = Screen::Prompt;
    }

    fn prompt_insert(&mut self, c: char) {
        let (buf, cursor) = insert_at(&self.prompt_buf, self.prompt_cursor, c);
        self.prompt_buf = buf;
        self.prompt_cursor = cursor;
    }

    fn prompt_backspace(&mut self) {
        let (buf, cursor) = backspace_at(&self.prompt_buf, self.prompt_cursor);
        self.prompt_buf = buf;
        self.prompt_cursor = cursor;
    }

    fn prompt_delete(&mut self) {
        let (buf, cursor) = delete_at(&self.prompt_buf, self.prompt_cursor);
        self.prompt_buf = buf;
        self.prompt_cursor = cursor;
    }

    fn begin(&mut self, title: &str) {
        let dest = self.config.dest_drive();
        let (dest_total, dest_used) = storage::disk_usage(&dest)
            .map(|(total, free)| (total, total.saturating_sub(free)))
            .unwrap_or((0, volume_used(self, &dest)));
        let (add, remove) = match title {
            "Syncing" => self
                .report
                .as_ref()
                .map(|report| sync::dest_space_delta(&self.config, report))
                .unwrap_or((0, 0)),
            "Cleanup" => {
                let (_, remove) = self
                    .report
                    .as_ref()
                    .map(|report| sync::dest_space_delta(&self.config, report))
                    .unwrap_or((0, 0));
                (0, remove)
            }
            _ => (0, 0),
        };
        let dest_projected = if add == 0 && remove == 0 {
            dest_used
        } else {
            dest_used.saturating_add(add).saturating_sub(remove)
        };
        let track_dest = matches!(
            title,
            "Syncing" | "Cleanup" | "Sync preview" | "Cleanup preview"
        );
        let now = Instant::now();
        *self.progress.lock().unwrap() = Progress {
            title: title.into(),
            dest_total: if track_dest { dest_total } else { 0 },
            dest_projected: if track_dest { dest_projected } else { 0 },
            dest_freed: if matches!(title, "Syncing" | "Cleanup") {
                remove
            } else {
                0
            },
            probe_used: if track_dest { dest_used } else { 0 },
            probe_done: 0,
            probe_at: if track_dest { Some(now) } else { None },
            started: Some(now),
            ..Progress::default()
        };
        self.remember_parent();
        self.screen = Screen::Loading;
        self.busy = true;
    }

    fn spawn<F>(&mut self, title: &str, work: F)
    where
        F: FnOnce(&mut dyn FnMut(&str, u64, u64)) -> TaskResult + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.begin(title);
        let progress = Arc::clone(&self.progress);
        self.task = Some(thread::spawn(move || {
            let mut cb = |msg: &str, done: u64, total: u64| note(&progress, msg, done, total);
            work(&mut cb)
        }));
    }

    fn poll_task(&mut self) {
        let Some(handle) = self.task.take() else {
            return;
        };
        if !handle.is_finished() {
            self.task = Some(handle);
            return;
        }
        self.busy = false;
        if self.screen == Screen::Loading {
            self.screen = self.overlay_back();
        }
        match handle.join() {
            Ok(TaskResult::Scan { config, report }) => {
                self.config = config;
                self.copy_jobs = report
                    .plan
                    .as_ref()
                    .map(|p| sync::build_copy_jobs(&self.config, p, &report.device_scans).len())
                    .unwrap_or(0);
                self.report = Some(report);
                self.refresh_scrobble_cache();
                if !self.config.new_walkman.is_empty() {
                    self.toast = format!("new Walkman: {}", self.config.new_walkman);
                }
            }
            Ok(TaskResult::Logs {
                title,
                logs,
                rescan,
            }) => {
                self.show_text(&title, logs);
                if rescan {
                    self.rescan();
                }
            }
            Ok(TaskResult::SyncPreview {
                logs,
                config,
                report,
            }) => {
                self.apply_dest_report(config, report);
                self.show_text("Sync preview — nothing written yet", logs);
                self.ask("Run the real sync now?", ConfirmAction::Sync);
            }
            Ok(TaskResult::CleanupPreview {
                logs,
                config,
                report,
            }) => {
                self.apply_dest_report(config, report);
                self.show_text("Cleanup preview", logs);
                self.ask("Delete the stale files listed above?", ConfirmAction::Cleanup);
            }
            Ok(TaskResult::AudioMuse(preview)) => {
                if !preview.skipped.is_empty() {
                    self.toast = preview.skipped;
                } else if !preview.error.is_empty() {
                    self.show_text("AudioMuse failed", audiomuse::preview_rows(&preview));
                } else {
                    self.show_text(
                        "AudioMuse preview — nothing written yet",
                        audiomuse::preview_rows(&preview),
                    );
                    self.ask(
                        "Write mixes and scores to the Walkman?",
                        ConfirmAction::AudioMuse,
                    );
                }
            }
            Ok(TaskResult::LikesReady { session, plan }) => {
                self.likes = Some(session);
                self.likes_plan = Some(plan);
                self.likes_idx = 0;
                self.screen = Screen::Likes;
            }
            Ok(TaskResult::LikesLogs {
                session,
                plan,
                title,
                logs,
            }) => {
                self.likes = Some(session);
                self.likes_plan = Some(plan);
                self.show_text(&title, logs);
            }
            Ok(TaskResult::SignedIn { session, name }) => {
                self.likes = Some(session);
                self.config.lastfm_username = name.clone();
                self.toast = format!("signed in as {name}");
                self.screen = Screen::LastFm;
            }
            Ok(TaskResult::SignedInFailed { session, error }) => {
                self.likes = Some(session);
                self.toast = error;
                self.screen = Screen::LastFm;
            }
            Err(_) => self.toast = "background task failed".into(),
        }
    }

    fn refresh_scrobble_cache(&mut self) {
        let files = sync::list_scrobble_files(&self.config);
        self.scrobble_pending = files.iter().map(|f| f.pending).sum();
        self.scrobble_note = if files.is_empty() {
            sync::missing_scrobble_message(self.config.is_stock()).into()
        } else {
            format!(
                "{} log(s) · {} play(s) to upload",
                files.len(),
                self.scrobble_pending
            )
        };
    }

    fn rescan(&mut self) {
        let mut config = self.config.clone();
        self.spawn("Scanning", move |p| {
            p("Detecting Walkman...", 0, 0);
            let _ = config.refresh_drives();
            let report = sync::build_scan_report(&config, false, p);
            TaskResult::Scan { config, report }
        });
    }

    fn dispatch(&mut self, action: &str) {
        if !action_enabled(&self.config, action) {
            self.toast = "not available on this Walkman".into();
            return;
        }
        match action {
            "quit" => self.should_quit = true,
            "sync" => self.do_sync(),
            "plan" => self.do_plan(),
            "refresh" => self.refresh_plan(),
            "playlists" => self.do_playlists(),
            "duplicates" => self.show_text("Duplicates", vec!["none".into()]),
            "likes" => self.start_likes_refresh(),
            "scrobbles" => self.do_scrobbles(),
            "audiomuse" => self.do_audiomuse(),
            "cleanup" => self.do_cleanup(),
            "index_library" => self.do_index_library(),
            "index_walkman" => self.do_index_walkman(),
            "settings" => {
                self.settings_idx = 0;
                self.screen = Screen::Settings;
            }
            "lastfm" => self.screen = Screen::LastFm,
            _ => self.toast = format!("not implemented: {action}"),
        }
    }

    fn ensure_likes(&mut self) {
        if self.likes.is_none() {
            self.likes = Some(likes::Session::new());
        }
    }

    fn refresh_likes(&mut self) {
        self.start_likes_refresh();
    }

    fn start_likes_refresh(&mut self) {
        if self.busy {
            return;
        }
        let mut session = self.likes.take().unwrap_or_else(likes::Session::new);
        self.spawn("Loading likes", move |p| {
            let mut status = |msg: &str| p(msg, 0, 0);
            let plan = session.refresh(&mut status);
            TaskResult::LikesReady { session, plan }
        });
    }

    fn start_likes_apply(&mut self, dry_run: bool) {
        if self.busy {
            return;
        }
        let mut session = self.likes.take().unwrap_or_else(likes::Session::new);
        let title = if dry_run {
            "Likes dry run"
        } else {
            "Applying likes"
        };
        self.spawn(title, move |p| {
            let mut status = |msg: &str| p(msg, 0, 0);
            let result = session.apply(dry_run, &mut status);
            let plan = session.refresh(&mut status);
            let mut logs = result.logs;
            if !result.lastfm_failed.is_empty() {
                logs.push(format!(
                    "{} Last.fm call(s) failed",
                    result.lastfm_failed.len()
                ));
            }
            TaskResult::LikesLogs {
                session,
                plan,
                title: if dry_run {
                    "Likes dry run".into()
                } else {
                    "Likes".into()
                },
                logs,
            }
        });
    }

    fn do_index_library(&mut self) {
        let config = self.config.clone();
        self.spawn("Indexing library", move |p| {
            let result = sync::index_library(&config, p);
            TaskResult::Logs {
                title: "Library index".into(),
                logs: result.logs,
                rescan: true,
            }
        });
    }

    fn do_index_walkman(&mut self) {
        let config = self.config.clone();
        self.spawn("Indexing Walkman", move |p| {
            let result = sync::index_walkman(&config, p);
            TaskResult::Logs {
                title: "Walkman index".into(),
                logs: result.logs,
                rescan: true,
            }
        });
    }

    fn apply_dest_report(&mut self, config: AppConfig, report: ScanReport) {
        self.copy_jobs = report
            .plan
            .as_ref()
            .map(|p| sync::build_copy_jobs(&config, p, &report.device_scans).len())
            .unwrap_or(0);
        self.config = config;
        self.report = Some(report);
    }

    fn do_sync(&mut self) {
        if self.report.is_none() {
            self.toast = "no scan data — press r".into();
            return;
        }
        let mut config = self.config.clone();
        self.spawn("Sync preview", move |p| {
            let report = dest_scan_report(&mut config, p);
            let preview = sync::sync_library(&config, &report, true, p);
            TaskResult::SyncPreview {
                logs: preview.logs,
                config,
                report,
            }
        });
    }

    fn do_cleanup(&mut self) {
        if self.report.is_none() {
            self.toast = "no scan data — press r".into();
            return;
        }
        let mut config = self.config.clone();
        self.spawn("Cleanup preview", move |p| {
            let report = dest_scan_report(&mut config, p);
            let preview = sync::cleanup_stale(&config, &report, true, p);
            TaskResult::CleanupPreview {
                logs: preview.logs,
                config,
                report,
            }
        });
    }

    fn refresh_plan(&mut self) {
        let mut config = self.config.clone();
        self.spawn("Refreshing plan", move |p| {
            let report = dest_scan_report(&mut config, p);
            TaskResult::Scan { config, report }
        });
    }

    fn do_plan(&mut self) {
        let Some(report) = &self.report else {
            self.toast = "no scan data — press r".into();
            return;
        };
        let diff = sync::plan_diff(&self.config, report);
        let mut rows = Vec::new();
        rows.push(plan_section_header("+", "Added", &diff.added));
        for album in &diff.added {
            rows.push(plan_change_row("+", album));
        }
        rows.push(String::new());
        rows.push(plan_section_header("~", "Modified", &diff.modified));
        for album in &diff.modified {
            rows.push(plan_change_row("~", album));
        }
        rows.push(String::new());
        rows.push(plan_section_header("-", "Removed", &diff.removed));
        for album in &diff.removed {
            rows.push(plan_change_row("-", album));
        }
        rows.push(String::new());
        rows.push(format!(
            "{} unchanged · {}",
            diff.unchanged,
            walkman::human_size(diff.unchanged_bytes)
        ));
        self.show_text("Plan", rows);
    }

    fn do_playlists(&mut self) {
        let Some(report) = &self.report else {
            self.toast = "no scan data — press r".into();
            return;
        };
        let mut rows = Vec::new();
        if let Some(plan) = &report.plan {
            for (name, tracks) in &plan.playlist {
                rows.push(format!("{name}  {} tracks", tracks.len()));
            }
        }
        if rows.is_empty() {
            rows.push("no playlists found".into());
        }
        self.show_text("Playlists", rows);
    }

    fn do_scrobbles(&mut self) {
        let files = sync::list_scrobble_files(&self.config);
        if files.is_empty() {
            self.show_text(
                "Scrobbles",
                vec![sync::missing_scrobble_message(self.config.is_stock()).into()],
            );
            return;
        }
        let rows: Vec<String> = files
            .iter()
            .map(|f| format!("{}: {} pending", f.path, f.pending))
            .collect();
        self.show_text("Scrobbles", rows);
        self.ask(
            "Upload these plays to Last.fm and clear them?",
            ConfirmAction::Scrobbles,
        );
    }

    fn do_audiomuse(&mut self) {
        self.config.pull_listen_credentials();
        if audiomuse::android_blocks_audiomuse(&self.config) {
            self.toast = "AudioMuse skipped (android-walkman)".into();
            return;
        }
        if !audiomuse::configured(&self.config) {
            self.toast = "AudioMuse not configured".into();
            return;
        }
        let config = self.config.clone();
        self.spawn("Fetching AudioMuse", move |p| {
            let mut status = |msg: &str| p(msg, 0, 0);
            TaskResult::AudioMuse(audiomuse::prepare(&config, &mut status))
        });
    }

    fn finish_confirm(&mut self) {
        match self.confirm_action {
            ConfirmAction::Sync => {
                let mut config = self.config.clone();
                self.spawn("Syncing", move |p| {
                    let report = dest_scan_report(&mut config, p);
                    let result = sync::sync_library(&config, &report, false, p);
                    TaskResult::Logs {
                        title: "Sync complete".into(),
                        logs: result.logs,
                        rescan: true,
                    }
                });
            }
            ConfirmAction::Cleanup => {
                let mut config = self.config.clone();
                self.spawn("Cleanup", move |p| {
                    let report = dest_scan_report(&mut config, p);
                    let result = sync::cleanup_stale(&config, &report, false, p);
                    TaskResult::Logs {
                        title: "Cleanup complete".into(),
                        logs: result.logs,
                        rescan: true,
                    }
                });
            }
            ConfirmAction::Scrobbles => {
                let config = self.config.clone();
                self.spawn("Uploading scrobbles", move |p| {
                    p("Uploading to Last.fm...", 0, 0);
                    let result = likes::upload_scrobbles(&config, false);
                    TaskResult::Logs {
                        title: "Scrobble upload complete".into(),
                        logs: result.logs,
                        rescan: false,
                    }
                });
            }
            ConfirmAction::AudioMuse => {
                let config = self.config.clone();
                self.spawn("Writing AudioMuse", move |p| {
                    p("Writing mixes and scores...", 0, 0);
                    let mut status = |msg: &str| p(msg, 0, 0);
                    let preview = audiomuse::prepare(&config, &mut status);
                    let result = audiomuse::commit(&preview, false);
                    TaskResult::Logs {
                        title: "AudioMuse complete".into(),
                        logs: audiomuse::preview_rows(&result),
                        rescan: false,
                    }
                });
            }
            ConfirmAction::LikesApply => self.start_likes_apply(false),
            ConfirmAction::None => self.screen = self.overlay_back(),
        }
        self.confirm_action = ConfirmAction::None;
    }

    fn apply_prompt(&mut self) {
        let kind = self.prompt_kind.clone();
        let answer = self.prompt_buf.trim().to_string();
        let back = self.overlay_back();
        if answer.is_empty() {
            self.screen = back;
            return;
        }
        match kind.as_str() {
            "source_dir" => self.config.settings.source_dir = answer,
            "playlist_dir" => self.config.settings.playlist_dir = answer,
            "name" => self.config.set_device_name(&answer),
            "dest" => {
                let v = answer.to_ascii_lowercase();
                if !matches!(v.as_str(), "internal" | "sd" | "auto") {
                    self.toast = "use internal, sd, or auto".into();
                    self.screen = Screen::Settings;
                    return;
                }
                self.config.set_dest(&v);
            }
            "kind" => {
                let Some(v) = walkman::canonical_device_kind(&answer) else {
                    self.toast = "use cinder, android-walkman, or stock".into();
                    self.screen = Screen::Settings;
                    return;
                };
                self.config.set_device_kind(v);
            }
            "internal" => self.config.set_internal_drive(&answer),
            "sd" => self.config.set_sd_drive(&answer),
            "internal_max" => {
                if let Ok(n) = answer.parse() {
                    self.config.set_internal_max_gb(n);
                }
            }
            "sd_max" => {
                if let Ok(n) = answer.parse() {
                    self.config.set_sd_max_gb(n);
                }
            }
            "workers" => {
                if let Ok(n) = answer.parse::<u32>() {
                    self.config.settings.copy_workers = n.max(1);
                }
            }
            "excluded" => self.config.settings.excluded_albums = answer,
            "lastfm_user" => {
                self.config.lastfm_username = answer.clone();
                paths::save_env_value("LASTFM_USERNAME", &answer);
            }
            "lastfm_key" => {
                self.config.lastfm_api_key = answer.clone();
                paths::save_env_value("LASTFM_API_KEY", &answer);
            }
            "lastfm_secret" => {
                self.config.lastfm_api_secret = answer.clone();
                paths::save_env_value("LASTFM_API_SECRET", &answer);
            }
            "lastfm_password" => {
                if self.busy {
                    return;
                }
                let user = self.config.lastfm_username.clone();
                let mut session = self.likes.take().unwrap_or_else(likes::Session::new);
                self.spawn("Signing in", move |p| {
                    p("Contacting Last.fm...", 0, 0);
                    match session.sign_in(&user, &answer) {
                        Ok(name) => TaskResult::SignedIn { session, name },
                        Err(err) => TaskResult::SignedInFailed {
                            session,
                            error: err.to_string(),
                        },
                    }
                });
                return;
            }
            _ => {}
        }
        let _ = self.config.persist();
        if self.toast.is_empty() {
            self.toast = "saved".into();
        }
        self.screen = back;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.busy || self.screen == Screen::Loading {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                self.should_quit = true;
            }
            return;
        }
        match self.screen {
            Screen::Dashboard => self.handle_dashboard(key.code),
            Screen::Settings => self.handle_settings(key.code),
            Screen::Text => self.handle_text(key.code),
            Screen::Likes => self.handle_likes(key.code),
            Screen::LastFm => self.handle_lastfm(key.code),
            Screen::Prompt => self.handle_prompt(key),
            Screen::Confirm => self.handle_confirm(key),
            Screen::Loading => {}
        }
    }

    fn handle_dashboard(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.menu > 0 {
                    self.menu -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.menu + 1 < ACTIONS.len() {
                    self.menu += 1;
                }
            }
            KeyCode::Enter => self.dispatch(ACTIONS[self.menu].2),
            KeyCode::Char('r') => self.refresh_plan(),
            KeyCode::Esc | KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char(c) => {
                if let Some((_, _, action)) = ACTIONS.iter().find(|(k, _, _)| *k == c.to_string()) {
                    self.dispatch(action);
                }
            }
            _ => {}
        }
    }

    fn handle_settings(&mut self, code: KeyCode) {
        let fields = settings_fields(&self.config);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.settings_idx > 0 {
                    self.settings_idx -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.settings_idx + 1 < fields.len() {
                    self.settings_idx += 1;
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Dashboard,
            KeyCode::Enter => {
                let (kind, label) = fields[self.settings_idx];
                match kind {
                    "index_library" => self.do_index_library(),
                    "index_walkman" => self.do_index_walkman(),
                    "detect" => {
                        self.config.settings.auto_detect_drives = true;
                        let changed = self.config.refresh_drives();
                        let _ = self.config.persist();
                        self.toast = if self.config.internal_drive().is_empty()
                            && self.config.sd_drive().is_empty()
                        {
                            "Walkman not found — plug it in and press Enter again".into()
                        } else {
                            format!(
                                "{}  |  {}{}",
                                walkman::describe_mount(&self.config.internal_drive()),
                                walkman::describe_mount(&self.config.sd_drive()),
                                if changed { " (updated)" } else { "" }
                            )
                        };
                    }
                    "known" => {}
                    _ => self.prompt(label, kind),
                }
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                self.screen = Screen::Dashboard;
            }
            _ => {}
        }
    }

    fn handle_likes(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.likes_idx = self.likes_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => self.likes_idx += 1,
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Dashboard,
            KeyCode::Char('r') => self.refresh_likes(),
            KeyCode::Char('d') => self.start_likes_apply(true),
            KeyCode::Char('a') => {
                self.ask(
                    "Apply these likes everywhere (writes to Last.fm and the device)?",
                    ConfirmAction::LikesApply,
                );
            }
            KeyCode::Char('c') => {
                self.ensure_likes();
                if let Some(session) = self.likes.as_mut() {
                    session.config.conflict = if session.config.conflict == "liked" {
                        "latest".into()
                    } else {
                        "liked".into()
                    };
                    session.config.save();
                    self.toast = format!("conflict rule: {} wins", session.config.conflict);
                    self.refresh_likes();
                }
            }
            _ => {}
        }
    }

    fn handle_lastfm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Dashboard,
            KeyCode::Char('i') => {
                if self.config.lastfm_username.is_empty() {
                    self.prompt("Last.fm username", "lastfm_user");
                } else {
                    self.prompt("Last.fm password", "lastfm_password");
                }
            }
            KeyCode::Char('k') => self.prompt("Last.fm API key", "lastfm_key"),
            KeyCode::Char('s') => self.prompt("Last.fm API secret", "lastfm_secret"),
            KeyCode::Char('u') => self.prompt("Last.fm username", "lastfm_user"),
            _ => {}
        }
    }

    fn handle_prompt(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.screen = self.overlay_back(),
            KeyCode::Enter => self.apply_prompt(),
            KeyCode::Backspace if ctrl => {
                self.prompt_buf.clear();
                self.prompt_cursor = 0;
            }
            KeyCode::Backspace => self.prompt_backspace(),
            KeyCode::Delete => self.prompt_delete(),
            KeyCode::Left => {
                self.prompt_cursor = self.prompt_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let n = self.prompt_buf.chars().count();
                if self.prompt_cursor < n {
                    self.prompt_cursor += 1;
                }
            }
            KeyCode::Home => self.prompt_cursor = 0,
            KeyCode::End => self.prompt_cursor = self.prompt_buf.chars().count(),
            KeyCode::Char('u') if ctrl => {
                self.prompt_buf.clear();
                self.prompt_cursor = 0;
            }
            KeyCode::Char(c) if !c.is_control() && !ctrl => self.prompt_insert(c),
            _ => {}
        }
    }

    fn cancel_confirm(&mut self) {
        self.confirm_action = ConfirmAction::None;
        self.confirm_buf.clear();
        self.toast = "cancelled".into();
        self.screen = self.overlay_back();
    }

    fn submit_confirm(&mut self) {
        let answer = self.confirm_buf.trim().to_ascii_lowercase();
        match answer.as_str() {
            "y" | "yes" => self.finish_confirm(),
            "n" | "no" | "" => self.cancel_confirm(),
            _ => {}
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_confirm(),
            KeyCode::Enter => self.submit_confirm(),
            KeyCode::Backspace => {
                self.confirm_buf.pop();
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if self.confirm_buf.is_empty() => {
                self.confirm_buf = "y".into();
                self.finish_confirm();
            }
            KeyCode::Char('n') | KeyCode::Char('N') if self.confirm_buf.is_empty() => {
                self.cancel_confirm();
            }
            KeyCode::Char(c) if !c.is_control() => self.confirm_buf.push(c),
            _ => {}
        }
    }
}

fn insert_at(buf: &str, cursor: usize, c: char) -> (String, usize) {
    let mut chars: Vec<char> = buf.chars().collect();
    let i = cursor.min(chars.len());
    chars.insert(i, c);
    (chars.into_iter().collect(), i + 1)
}

fn backspace_at(buf: &str, cursor: usize) -> (String, usize) {
    if cursor == 0 {
        return (buf.to_string(), 0);
    }
    let mut chars: Vec<char> = buf.chars().collect();
    let i = cursor.min(chars.len());
    chars.remove(i - 1);
    (chars.into_iter().collect(), i - 1)
}

fn delete_at(buf: &str, cursor: usize) -> (String, usize) {
    let mut chars: Vec<char> = buf.chars().collect();
    if cursor < chars.len() {
        chars.remove(cursor);
    }
    let len = chars.len();
    (chars.into_iter().collect(), cursor.min(len))
}

fn prompt_seed(app: &App, kind: &str) -> String {
    match kind {
        "lastfm_user" => app.config.lastfm_username.clone(),
        "lastfm_key" => app.config.lastfm_api_key.clone(),
        "lastfm_secret" => app.config.lastfm_api_secret.clone(),
        "lastfm_password" => String::new(),
        "internal" => app.config.internal_drive(),
        "sd" => app.config.sd_drive(),
        other => {
            let value = settings_value(app, other);
            if value == "⏎ run" {
                String::new()
            } else {
                value
            }
        }
    }
}

fn prompt_shown(buf: &str, hidden: bool) -> String {
    if hidden {
        "•".repeat(buf.chars().count())
    } else {
        buf.to_string()
    }
}

fn prompt_line(buf: &str, cursor: usize, hidden: bool, caret_on: bool) -> Line<'static> {
    let shown: Vec<char> = prompt_shown(buf, hidden).chars().collect();
    let i = cursor.min(shown.len());
    let mut spans = Vec::new();
    if i > 0 {
        spans.push(Span::raw(shown[..i].iter().collect::<String>()));
    }
    let caret_ch = shown.get(i).copied().unwrap_or(' ');
    let caret_style = if caret_on {
        Style::default()
            .fg(Color::Black)
            .bg(AMBER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(AMBER)
    };
    spans.push(Span::styled(caret_ch.to_string(), caret_style));
    if i + 1 < shown.len() {
        spans.push(Span::raw(shown[i + 1..].iter().collect::<String>()));
    }
    Line::from(spans)
}

fn spinner_frame(ms: u128) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(ms / 80) as usize % FRAMES.len()]
}

fn elapsed_ms(p: &Progress) -> u128 {
    p.started
        .map(|t| t.elapsed().as_millis())
        .unwrap_or(0)
}

fn infer_unit(total: u64) -> ProgressUnit {
    if total >= 1024 * 1024 {
        ProgressUnit::Bytes
    } else {
        ProgressUnit::Count
    }
}

fn note(progress: &Arc<Mutex<Progress>>, msg: &str, done: u64, total: u64) {
    let mut p = progress.lock().unwrap();
    p.message = msg.to_string();
    if p.started.is_none() {
        p.started = Some(Instant::now());
    }
    if total > 0 {
        if p.total != total {
            p.bps = None;
            p.ticks = 0;
            p.last_at = None;
            p.last_bytes = done;
            p.eta_secs = None;
            p.eta_at = None;
            p.unit = infer_unit(total);
        }
        p.done = done;
        p.total = total;
        sample_rate(&mut p, done);
    }
    if p.lines.last().map(|s| s.as_str()) != Some(msg) {
        p.lines.push(msg.to_string());
        if p.lines.len() > 200 {
            let extra = p.lines.len() - 200;
            p.lines.drain(..extra);
        }
    }
}

fn sample_rate(p: &mut Progress, done: u64) {
    let now = Instant::now();
    if p.last_at.is_none() {
        p.last_at = Some(now);
        p.last_bytes = done;
        return;
    }
    let Some(last_at) = p.last_at else {
        return;
    };
    let dt = now.duration_since(last_at).as_secs_f64();
    let db = done.saturating_sub(p.last_bytes) as f64;
    if db <= 0.0 {
        return;
    }
    let tiny = match p.unit {
        ProgressUnit::Bytes => db < 64.0 * 1024.0,
        ProgressUnit::Count => false,
    };
    if dt >= 2.0 && tiny {
        p.last_at = Some(now);
        p.last_bytes = done;
        return;
    }
    if dt < 0.5 {
        commit_eta(p);
        return;
    }
    let inst = db / dt;
    if p.unit == ProgressUnit::Bytes {
        const MAX_BPS: f64 = 200.0 * 1024.0 * 1024.0;
        if inst >= MAX_BPS {
            p.last_at = Some(now);
            p.last_bytes = done;
            return;
        }
    }
    p.last_at = Some(now);
    p.last_bytes = done;
    p.bps = Some(match p.bps {
        Some(prev) => prev * 0.95 + inst * 0.05,
        None => inst,
    });
    p.ticks = p.ticks.saturating_add(1);
    commit_eta(p);
}

fn blended_rate(p: &Progress) -> Option<f64> {
    let ema = p.bps?;
    let elapsed = elapsed_secs(p);
    if elapsed >= 2.0 && p.done > 0 {
        let overall = p.done as f64 / elapsed;
        Some(ema * 0.6 + overall * 0.4)
    } else {
        Some(ema)
    }
}

fn min_rate(p: &Progress) -> f64 {
    match p.unit {
        ProgressUnit::Bytes => 512.0,
        ProgressUnit::Count => 0.05,
    }
}

fn commit_eta(p: &mut Progress) {
    if p.total == 0 || p.done >= p.total {
        return;
    }
    let Some(rate) = blended_rate(p).filter(|b| *b > min_rate(p)) else {
        return;
    };
    let raw = p.total.saturating_sub(p.done) as f64 / rate;
    if !raw.is_finite() || raw < 0.0 {
        return;
    }
    let now = Instant::now();
    let Some(prev) = p.eta_secs else {
        p.eta_secs = Some(raw);
        p.eta_at = Some(now);
        return;
    };
    let ticked = match p.eta_at {
        Some(at) => (prev - at.elapsed().as_secs_f64()).max(0.0),
        None => prev,
    };
    if raw < ticked || (raw > ticked * 1.25 && raw - ticked > 15.0) {
        p.eta_secs = Some(raw);
        p.eta_at = Some(now);
    }
}

fn elapsed_secs(p: &Progress) -> f64 {
    p.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
}

fn displayed_eta_secs(p: &Progress) -> Option<f64> {
    let secs = p.eta_secs?;
    let at = p.eta_at?;
    Some((secs - at.elapsed().as_secs_f64()).max(0.0))
}

fn eta_label(p: &Progress) -> Option<(String, Color)> {
    if p.total == 0 {
        return None;
    }
    if p.done >= p.total {
        return Some(("finishing".into(), Color::DarkGray));
    }
    let elapsed = elapsed_secs(p);
    if p.ticks < 1 || elapsed < 0.4 {
        return Some(("estimating…".into(), Color::DarkGray));
    }
    let Some(secs) = displayed_eta_secs(p) else {
        return Some(("estimating…".into(), Color::DarkGray));
    };
    Some((walkman::human_eta_display(secs), AMBER))
}

fn rate_label(p: &Progress) -> Option<String> {
    let rate = blended_rate(p).filter(|b| *b > 0.0)?;
    match p.unit {
        ProgressUnit::Bytes if rate > 512.0 => Some(walkman::human_rate(rate)),
        ProgressUnit::Count if rate >= 10.0 => Some(format!("{rate:.0}/s")),
        ProgressUnit::Count if rate >= 0.05 => Some(format!("{rate:.1}/s")),
        _ => None,
    }
}

fn meter_label(p: &Progress) -> String {
    if p.total == 0 {
        return "working…".into();
    }
    let ratio = (p.done as f64 / p.total as f64).clamp(0.0, 1.0);
    let amounts = match p.unit {
        ProgressUnit::Bytes => format!(
            "{} / {}",
            walkman::human_size(p.done),
            walkman::human_size(p.total)
        ),
        ProgressUnit::Count => format!("{} / {}", p.done, p.total),
    };
    let mut parts = vec![format!("{:.0}%", ratio * 100.0), amounts];
    if let Some(rate) = rate_label(p) {
        parts.push(rate);
    }
    parts.join("  ")
}

fn tagged_line(raw: &str) -> Line<'static> {
    let (mark, rest, color) = if let Some(rest) = raw.strip_prefix("+ ") {
        ("+", rest, Color::Green)
    } else if let Some(rest) = raw.strip_prefix("- ") {
        ("-", rest, Color::Red)
    } else if let Some(rest) = raw.strip_prefix("~ ") {
        ("~", rest, Color::Yellow)
    } else {
        return Line::from(raw.to_string());
    };
    Line::from(vec![
        Span::styled(
            format!("{mark} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(rest.to_string()),
    ])
}

fn signed_size(delta: i64) -> String {
    if delta > 0 {
        format!("+{}", walkman::human_size(delta as u64))
    } else if delta < 0 {
        format!("-{}", walkman::human_size(delta.unsigned_abs()))
    } else {
        "0 B".into()
    }
}

fn files_label(add: u32, remove: u32) -> String {
    match (add, remove) {
        (0, 0) => "0 files".into(),
        (a, 0) => format!("+{a} files"),
        (0, r) => format!("-{r} files"),
        (a, r) => format!("+{a} / -{r} files"),
    }
}

fn plan_section_header(mark: &str, title: &str, rows: &[sync::AlbumChange]) -> String {
    let add: u32 = rows.iter().map(|a| a.add_files).sum();
    let remove: u32 = rows.iter().map(|a| a.remove_files).sum();
    let net: i64 = rows.iter().map(|a| a.net_bytes()).sum();
    format!(
        "{mark} {title:<9} {} albums   {}   {}",
        rows.len(),
        signed_size(net),
        files_label(add, remove)
    )
}

fn plan_change_row(mark: &str, album: &sync::AlbumChange) -> String {
    format!(
        "{mark} {}  {}  {}",
        album.name,
        signed_size(album.net_bytes()),
        files_label(album.add_files, album.remove_files)
    )
}

fn is_internal(path: &str, internal: &str) -> bool {
    storage::fold_path(path) == storage::fold_path(internal)
}

fn dest_scan_report(
    config: &mut AppConfig,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> ScanReport {
    progress("Detecting Walkman...", 0, 0);
    let _ = config.refresh_drives();
    sync::build_scan_report(config, true, progress)
}

fn settings_fields(config: &AppConfig) -> Vec<(&'static str, &'static str)> {
    let _ = config;
    vec![
        ("index_library", "Index library"),
        ("index_walkman", "Index Walkman"),
        ("source_dir", "Source library"),
        ("playlist_dir", "Playlist folder"),
        ("name", "Walkman name"),
        ("known", "Other Walkmans"),
        ("dest", "Destination (internal/sd/auto)"),
        ("kind", "Device (cinder / android-walkman / stock)"),
        ("detect", "Walkman mounts"),
        ("internal", "Internal"),
        ("sd", "SD card"),
        ("internal_max", "Internal limit GB (0=live)"),
        ("sd_max", "SD limit GB (0=live)"),
        ("workers", "Copy workers"),
        ("excluded", "Excluded albums"),
    ]
}

fn settings_value(app: &App, kind: &str) -> String {
    match kind {
        "index_library" | "index_walkman" => "⏎ run".into(),
        "source_dir" => app.config.source_dir(),
        "playlist_dir" => app.config.playlist_dir(),
        "name" => app.config.device_name(),
        "known" => {
            let others = app.config.settings.other_device_names();
            if others.is_empty() {
                "none".into()
            } else {
                others.join(", ")
            }
        }
        "dest" => app.config.dest_choice(),
        "kind" => app.config.settings.device_kind(),
        "detect" => {
            if app.config.settings.auto_detect_drives {
                "auto — ⏎ refresh".into()
            } else {
                "pinned — ⏎ re-enable auto".into()
            }
        }
        "internal" => walkman::describe_mount(&app.config.internal_drive()),
        "sd" => walkman::describe_mount(&app.config.sd_drive()),
        "internal_max" => app
            .config
            .settings
            .active_profile()
            .map(|p| p.internal_max_gb.to_string())
            .unwrap_or_else(|| "0".into()),
        "sd_max" => app
            .config
            .settings
            .active_profile()
            .map(|p| p.sd_max_gb.to_string())
            .unwrap_or_else(|| "0".into()),
        "workers" => app.config.settings.copy_workers.to_string(),
        "excluded" => app.config.settings.excluded_albums.clone(),
        _ => String::new(),
    }
}

fn likes_rows(plan: &likes::Plan) -> Vec<String> {
    let mut rows = vec!["Sources".into()];
    for name in ["device", "lastfm", "musicbee"] {
        if let Some(view) = plan.views.get(name) {
            let mark = if view.available { "ok" } else { "--" };
            rows.push(format!(
                "  {mark} {name:<10} {:>4} liked  {}",
                view.count(),
                view.note
            ));
        }
    }
    rows.push(String::new());
    rows.push(format!("Merged list: {} liked track(s)", plan.desired.len()));
    for name in ["device", "lastfm", "musicbee"] {
        if let Some(sink) = plan.sinks.get(name) {
            if !sink.enabled {
                rows.push(format!("  {name:<10} skipped — {}", sink.note));
                continue;
            }
            rows.push(format!(
                "  {name:<10} +{}  -{}",
                sink.add.len(),
                sink.remove.len()
            ));
            for key in sink.add.iter().take(80) {
                rows.push(format!("      + {}", plan.label(key)));
            }
            for key in sink.remove.iter().take(80) {
                rows.push(format!("      - {}", plan.label(key)));
            }
        }
    }
    for note in &plan.notes {
        rows.push(format!("  {note}"));
    }
    rows
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if app.busy || app.screen == Screen::Loading {
        draw_progress(frame, area, app);
        return;
    }
    match app.screen {
        Screen::Dashboard => draw_dashboard(frame, area, app),
        Screen::Settings => draw_settings(frame, area, app),
        Screen::Text => draw_text(frame, area, app),
        Screen::Likes => draw_likes(frame, area, app),
        Screen::LastFm => draw_lastfm(frame, area, app),
        Screen::Prompt => {
            draw_parent(frame, area, app);
            draw_prompt(frame, area, app);
        }
        Screen::Confirm => {
            draw_parent(frame, area, app);
            draw_confirm(frame, area, app);
        }
        Screen::Loading => draw_progress(frame, area, app),
    }
}

fn draw_parent(frame: &mut Frame, area: Rect, app: &App) {
    match app.overlay_parent {
        Screen::Settings => draw_settings(frame, area, app),
        Screen::Text => draw_text(frame, area, app),
        Screen::Likes => draw_likes(frame, area, app),
        Screen::LastFm => draw_lastfm(frame, area, app),
        _ => draw_dashboard(frame, area, app),
    }
}

fn header<'a>(app: &'a App) -> Line<'a> {
    let connected = app.config.dest_is_connected();
    let device = if connected { "connected" } else { "not connected" };
    let color = if connected { Color::Green } else { Color::DarkGray };
    Line::from(vec![
        Span::styled(
            " WALKMAN SYNC ",
            Style::default()
                .fg(Color::Black)
                .bg(AMBER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  walkman "),
        Span::styled(
            app.config.device_name(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {device}   "), Style::default().fg(color)),
        Span::raw("last.fm "),
        Span::styled(
            if app.config.lastfm_username.is_empty() {
                "not signed in"
            } else {
                app.config.lastfm_username.as_str()
            },
            if app.config.lastfm_username.is_empty() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
    ])
}

fn bar(fraction: f64, width: usize) -> String {
    let width = width.max(4);
    let frac = fraction.clamp(0.0, 1.0);
    let filled = ((frac * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn dest_bar_segments(current: u64, projected: u64, total: u64, width: usize) -> (usize, usize, usize) {
    let width = width.max(1);
    let total = total.max(1) as f64;
    let cur = ((current as f64 / total) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let proj = ((projected as f64 / total) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let solid = cur.min(width);
    let ghost = proj.saturating_sub(cur).min(width.saturating_sub(solid));
    let empty = width.saturating_sub(solid + ghost);
    (solid, ghost, empty)
}

fn dest_capacity_line(current: u64, projected: u64, total: u64, width: usize) -> Line<'static> {
    let (solid, ghost, empty) = dest_bar_segments(current, projected, total, width);
    let mut spans = Vec::new();
    if solid > 0 {
        spans.push(Span::styled(
            "█".repeat(solid),
            Style::default().fg(DEST_SOLID).bg(DEST_SOLID),
        ));
    }
    if ghost > 0 {
        spans.push(Span::styled(
            "█".repeat(ghost),
            Style::default().fg(DEST_GHOST).bg(Color::Rgb(12, 36, 18)),
        ));
    }
    if empty > 0 {
        spans.push(Span::styled(
            "░".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn live_dest_used(p: &Progress) -> u64 {
    let cap = if p.dest_total > 0 { p.dest_total } else { u64::MAX };
    let base = p.probe_used.saturating_sub(p.dest_freed);
    if p.unit != ProgressUnit::Bytes {
        return base.min(cap);
    }
    let extra = p.done.saturating_sub(p.probe_done);
    base.saturating_add(extra).min(cap)
}

fn should_probe_dest(busy: bool, dest: &str) -> bool {
    if dest.is_empty() {
        return false;
    }
    !(busy && storage::is_remote(dest))
}

fn maybe_probe_dest(app: &App) {
    let dest = app.config.dest_drive();
    if !should_probe_dest(app.busy, &dest) {
        return;
    }
    let mut p = app.progress.lock().unwrap();
    if p.dest_total == 0 {
        return;
    }
    let now = Instant::now();
    let since = p
        .probe_at
        .map(|t| now.duration_since(t))
        .unwrap_or(Duration::MAX);
    if since < Duration::from_millis(800) {
        return;
    }
    let jumped = p.done.abs_diff(p.probe_done) >= 8 * 1024 * 1024;
    if !jumped && since < Duration::from_secs(2) {
        return;
    }
    let Some((total, free)) = storage::disk_usage(&dest) else {
        return;
    };
    p.dest_total = total.max(1);
    let used = total.saturating_sub(free);
    p.probe_used = used;
    p.probe_done = p.done;
    p.probe_at = Some(now);
}

fn bar_color(fraction: f64) -> Color {
    if fraction >= 0.9 {
        Color::Red
    } else if fraction >= 0.7 {
        AMBER
    } else {
        Color::Green
    }
}

fn volume_used(app: &App, path: &str) -> u64 {
    if path.is_empty() {
        return 0;
    }
    if let Some((total, free)) = storage::disk_usage(path) {
        return total.saturating_sub(free);
    }
    app.report
        .as_ref()
        .and_then(|r| r.device_scans.get(path))
        .map(|s| s.total_size())
        .unwrap_or(0)
}

fn volume_cap(path: &str, used: u64, max_gb: f64) -> u64 {
    if max_gb > 0.0 {
        (max_gb * 1024.0 * 1024.0 * 1024.0) as u64
    } else {
        walkman::live_total_bytes(path).max(used)
    }
}

fn dest_mark<'a>(is_dest: bool) -> Span<'a> {
    if is_dest {
        Span::styled(" dest", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" unused", Style::default().fg(Color::DarkGray))
    }
}

fn volume_file_count(app: &App, path: &str, kind: &str) -> Option<u64> {
    if path.is_empty() {
        return None;
    }
    if let Some(n) = app
        .report
        .as_ref()
        .and_then(|r| r.catalog_coverage.get(kind))
        .and_then(|v| v.as_ref())
        .map(|v| v.file_count as u64)
    {
        return Some(n);
    }
    app.report
        .as_ref()
        .and_then(|r| r.device_scans.get(path))
        .map(|s| s.files.len() as u64)
}

fn storage_line<'a>(
    label: &'a str,
    used: u64,
    total: u64,
    files: Option<u64>,
    is_dest: bool,
) -> Line<'a> {
    let frac = if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    };
    let mut sizes = if total == 0 {
        if used == 0 {
            "  —".to_string()
        } else {
            format!("  {}", walkman::human_size(used))
        }
    } else {
        format!(
            "  {} / {}",
            walkman::human_size(used),
            walkman::human_size(total)
        )
    };
    if let Some(n) = files {
        sizes.push_str(&format!("  {n} files"));
    }
    sizes.push(' ');
    Line::from(vec![
        Span::raw(format!("{label:<10}")),
        Span::styled(bar(frac, 16), Style::default().fg(bar_color(frac))),
        Span::raw(sizes),
        dest_mark(is_dest),
    ])
}

fn panel_block(title: &str, accent: Color) -> Block<'_> {
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
}

fn draw_dashboard(frame: &mut Frame, area: Rect, app: &App) {
    let narrow = area.width < 72;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if narrow { 16 } else { 8 }),
            Constraint::Length(if narrow { 14 } else { 7 }),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(header(app)).style(Style::default().bg(Color::Rgb(36, 36, 36))),
        chunks[0],
    );

    let dest = app.config.dest_drive();
    let internal = app.config.internal_drive();
    let sd = app.config.sd_drive();
    let dest_internal = is_internal(&dest, &internal);
    let internal_used = volume_used(app, &internal);
    let sd_used = volume_used(app, &sd);
    let profile = app.config.settings.active_profile();
    let internal_cap = volume_cap(
        &internal,
        internal_used,
        profile.map(|p| p.internal_max_gb).unwrap_or(0.0),
    );
    let sd_cap = volume_cap(
        &sd,
        sd_used,
        profile.map(|p| p.sd_max_gb).unwrap_or(0.0),
    );
    let albums = app
        .report
        .as_ref()
        .and_then(|r| r.plan.as_ref())
        .map(|p| p.source_artists.len())
        .unwrap_or(0);
    let source_bytes = app
        .report
        .as_ref()
        .map(|r| r.source_total_bytes)
        .unwrap_or(0);
    let index_line = app
        .report
        .as_ref()
        .map(|r| sync::catalog_index_line(&app.config, r))
        .unwrap_or_else(|| "Library index never · Walkman index never".into());
    let excluded = app.config.excluded_albums();
    let mut storage_lines = vec![
        storage_line(
            "Internal",
            internal_used,
            internal_cap,
            volume_file_count(app, &internal, catalog::KIND_INTERNAL),
            dest_internal,
        ),
        storage_line(
            "SD card",
            sd_used,
            sd_cap,
            volume_file_count(app, &sd, catalog::KIND_SD),
            !dest_internal && !sd.is_empty(),
        ),
        Line::from(""),
        Line::from(vec![
            Span::raw("Source    "),
            Span::styled(
                walkman::human_size(source_bytes),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!("   {albums} albums")),
        ]),
        Line::from(vec![
            Span::raw("Excluded  "),
            Span::styled(
                if excluded.is_empty() {
                    "none".into()
                } else {
                    format!("{} album(s)", excluded.len())
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(index_line, Style::default().fg(Color::DarkGray))),
    ];
    if dest_internal && sd_used > 0 {
        storage_lines.push(Line::from(Span::styled(
            "SD leftover will be cleared",
            Style::default().fg(Color::Yellow),
        )));
    }
    if !dest_internal && internal_used > 0 {
        storage_lines.push(Line::from(Span::styled(
            "Internal leftover will be cleared",
            Style::default().fg(Color::Yellow),
        )));
    }

    let change_lines = sync_change_lines(app);
    let likes_lines = likes_summary_lines(app);
    let scrobble_lines = vec![
        Line::from(Span::styled(
            app.scrobble_note.clone(),
            if app.scrobble_pending > 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )),
        Line::from(""),
        Line::from(Span::styled(
            "6  upload to Last.fm",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let storage_p = Paragraph::new(storage_lines).block(panel_block("Storage", Color::Cyan));
    let change_p = Paragraph::new(change_lines).block(panel_block("This sync", AMBER));
    let likes_p = Paragraph::new(likes_lines).block(panel_block("Likes", Color::Magenta));
    let scrobble_p = Paragraph::new(scrobble_lines).block(panel_block("Scrobbles", Color::Green));

    if narrow {
        let stacked = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);
        frame.render_widget(storage_p, stacked[0]);
        frame.render_widget(change_p, stacked[1]);
        let stacked = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        frame.render_widget(likes_p, stacked[0]);
        frame.render_widget(scrobble_p, stacked[1]);
    } else {
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);
        frame.render_widget(storage_p, top[0]);
        frame.render_widget(change_p, top[1]);
        let mid = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);
        frame.render_widget(likes_p, mid[0]);
        frame.render_widget(scrobble_p, mid[1]);
    }

    let items: Vec<ListItem> = ACTIONS
        .iter()
        .enumerate()
        .map(|(i, (k, label, action))| {
            let enabled = action_enabled(&app.config, action);
            let style = menu_item_style(i == app.menu, enabled);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {k}  {label}"), style),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.menu));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Menu "))
            .highlight_symbol("▸"),
        chunks[3],
        &mut state,
    );

    let footer = if app.toast.is_empty() {
        " ↑↓ move   ⏎ choose   r refresh plan   / 1-9 i w s l   q quit".to_string()
    } else {
        format!(" ↑↓  ⏎  r refresh   q quit   {}", app.toast)
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(AMBER)),
        chunks[4],
    );
}

fn sync_change_lines(app: &App) -> Vec<Line<'static>> {
    let Some(report) = &app.report else {
        return vec![Line::from(Span::styled(
            "no scan yet — press r",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let Some(plan) = &report.plan else {
        return vec![Line::from(Span::styled(
            "no plan — press r",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let diff = sync::plan_diff(&app.config, report);
    let additions = diff.added.len();
    let modified = diff.modified.len();
    let deletions = diff.removed.len();
    let net = additions as i64 - deletions as i64;
    let net_size = diff.net_bytes();
    let jobs = diff.add_files() as usize;
    let remove_files = diff.remove_files() as usize;
    let copy_frac = if jobs + remove_files == 0 {
        0.0
    } else {
        jobs as f64 / (jobs + remove_files).max(1) as f64
    };

    let mut lines = vec![
        Line::from(vec![
            Span::raw("Net       "),
            Span::styled(
                format!(
                    "{}{} albums",
                    if net >= 0 { "+" } else { "" },
                    net
                ),
                Style::default()
                    .fg(if net >= 0 { Color::Green } else { Color::Red })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "{}{}",
                if net_size >= 0 { "+" } else { "-" },
                walkman::human_size(net_size.unsigned_abs())
            )),
        ]),
        Line::from(vec![
            Span::styled(format!("+{additions}"), Style::default().fg(Color::Green)),
            Span::raw(" add   "),
            Span::styled(format!("~{modified}"), Style::default().fg(Color::Yellow)),
            Span::raw(" modify   "),
            Span::styled(format!("-{deletions}"), Style::default().fg(Color::Red)),
            Span::raw(" remove"),
        ]),
        Line::from(vec![
            Span::styled(bar(copy_frac.max(0.05), 22), Style::default().fg(AMBER)),
            Span::raw("  copy vs remove"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("Files to copy   "),
            Span::styled(
                jobs.to_string(),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("Files to remove "),
            Span::styled(
                remove_files.to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!(
            "Playlists       {} placed",
            plan.playlist.len()
        )),
    ];
    let dest = app.config.dest_drive();
    let held = report
        .device_scans
        .get(&dest)
        .map(|s| s.total_size())
        .unwrap_or(0);
    let budget = app.config.dest_budget_bytes(held);
    if budget > 0 && source_planned_bytes(plan) > budget {
        lines.push(Line::from(Span::styled(
            format!(
                "OVERFLOW short {}",
                walkman::human_size(source_planned_bytes(plan).saturating_sub(budget))
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn source_planned_bytes(plan: &sync::LibraryPlan) -> u64 {
    plan.assignments
        .keys()
        .map(|name| {
            plan.source_files
                .get(name)
                .map(|v| v.iter().map(|e| e.size).sum::<u64>())
                .unwrap_or(0)
        })
        .sum()
}

fn likes_summary_lines(app: &App) -> Vec<Line<'static>> {
    let Some(plan) = &app.likes_plan else {
        return vec![
            Line::from(Span::styled(
                "not loaded — open Likes (5)",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "device ⇄ Last.fm ⇄ MusicBee",
                Style::default().fg(Color::DarkGray),
            )),
        ];
    };
    let mut lines = vec![Line::from(format!(
        "Merged    {} liked track(s)",
        plan.desired.len()
    ))];
    for name in ["device", "lastfm", "musicbee"] {
        let view = plan.views.get(name);
        let sink = plan.sinks.get(name);
        let available = view.map(|v| v.available).unwrap_or(false);
        let count = view.map(|v| v.count()).unwrap_or(0);
        let mut spans = vec![
            Span::styled(
                if available { "ok " } else { "-- " },
                if available {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw(format!("{name:<9} {count:>4}")),
        ];
        if let Some(sink) = sink {
            if available && sink.enabled && (!sink.add.is_empty() || !sink.remove.is_empty()) {
                spans.push(Span::styled(
                    format!("  +{}", sink.add.len()),
                    Style::default().fg(Color::Green),
                ));
                spans.push(Span::styled(
                    format!(" -{}", sink.remove.len()),
                    Style::default().fg(Color::Red),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    let on_device: usize = plan.playlist.values().map(|p| p.len()).sum();
    lines.push(Line::from(format!(
        "Playlist  {on_device} track(s) on the Walkman"
    )));
    lines
}

fn draw_settings(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(4), Constraint::Length(2)])
        .split(area);
    frame.render_widget(Paragraph::new(header(app)), chunks[0]);
    let fields = settings_fields(&app.config);
    let items: Vec<ListItem> = fields
        .iter()
        .enumerate()
        .map(|(i, (kind, label))| {
            let marker = if i == app.settings_idx { "▸ " } else { "  " };
            let value = settings_value(app, kind);
            let style = if i == app.settings_idx {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(
                format!("{marker}{label:<32} {value}"),
                style,
            ))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().title(" Settings ").borders(Borders::ALL)),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(" ⏎ edit / refresh mounts   esc back"),
        chunks[2],
    );
}

fn draw_text(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)])
        .split(area);
    frame.render_widget(Paragraph::new(header(app)), chunks[0]);
    let start = app.scroll.min(app.text_rows.len().saturating_sub(1));
    let body: Vec<Line> = app.text_rows[start..]
        .iter()
        .map(|s| tagged_line(s))
        .collect();
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(format!(" {} ", app.text_title))
                    .borders(Borders::ALL),
            ),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(" ↑↓ scroll   esc back"), chunks[2]);
}

fn draw_likes(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)])
        .split(area);
    frame.render_widget(Paragraph::new(header(app)), chunks[0]);
    let rows = app
        .likes_plan
        .as_ref()
        .map(likes_rows)
        .unwrap_or_else(|| vec!["not loaded".into()]);
    let start = app.likes_idx.min(rows.len().saturating_sub(1));
    frame.render_widget(
        Paragraph::new(rows[start..].join("\n")).block(
            Block::default()
                .title(" Likes — device ⇄ Last.fm ⇄ MusicBee ")
                .borders(Borders::ALL),
        ),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(" a apply   d dry run   r reload   c conflict   esc back"),
        chunks[2],
    );
}

fn draw_lastfm(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)])
        .split(area);
    frame.render_widget(Paragraph::new(header(app)), chunks[0]);
    let user = if app.config.lastfm_username.is_empty() {
        "not signed in".into()
    } else {
        app.config.lastfm_username.clone()
    };
    let body = format!(
        "User    {user}\nAPI key {}\nSecret  {}\nSession {}\n\ni sign in   k api key   s api secret   u username   esc back",
        mask(&app.config.lastfm_api_key),
        mask(&app.config.lastfm_api_secret),
        if app.config.lastfm_session_key.is_empty() {
            "missing"
        } else {
            "ready"
        }
    );
    frame.render_widget(
        Paragraph::new(body).block(Block::default().title(" Last.fm account ").borders(Borders::ALL)),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(" esc back"), chunks[2]);
}

fn mask(value: &str) -> String {
    if value.is_empty() {
        "(empty)".into()
    } else if value.len() <= 4 {
        "****".into()
    } else {
        format!("{}…", &value[..4])
    }
}

const ART_MIN_LOGS: u16 = 42;
const ART_MIN_SIDE: u16 = 16;

/// Square art panel on the right of `body`. Terminal cells are ~1:2 and each
/// cell is two stacked pixels, so a square cover is `width ≈ 2 * height`.
fn art_panel_rect(body: Rect) -> Option<Rect> {
    if body.height < 8 {
        return None;
    }
    let max_w = body.width.saturating_sub(ART_MIN_LOGS);
    if max_w < ART_MIN_SIDE {
        return None;
    }
    let mut height = body.height;
    let mut width = height.saturating_mul(2).saturating_sub(2);
    if width > max_w {
        width = max_w;
        height = (width + 2) / 2;
    }
    if height < 8 || width < ART_MIN_SIDE || height > body.height {
        return None;
    }
    Some(Rect {
        x: body.x + body.width.saturating_sub(width),
        y: body.y,
        width,
        height,
    })
}

fn meter_stack_height(show_dest: bool) -> u16 {
    3 + 3 + if show_dest { 3 } else { 0 }
}

fn current_audio_path(config: &AppConfig, message: &str) -> Option<PathBuf> {
    let (root, rel) = if let Some(rel) = message.strip_prefix("+ ") {
        (config.source_dir(), rel)
    } else if let Some(rel) = message.strip_prefix("~ ") {
        (config.source_dir(), rel)
    } else if let Some(rel) = message.strip_prefix("- ") {
        (config.dest_drive(), rel)
    } else {
        return None;
    };
    let name = Path::new(rel).file_name().and_then(|s| s.to_str())?;
    if !storage::is_audio(name) {
        return None;
    }
    Some(PathBuf::from(storage::join(&root, &[rel])))
}

fn prepare_art(app: &App, path: &Path, width: u16, height: u16) {
    if width == 0 || height == 0 {
        return;
    }
    let album = path
        .parent()
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let key = (album, width, height);
    {
        let cache = app.art.lock().unwrap();
        if cache.key.as_ref() == Some(&key) {
            return;
        }
    }
    let raster = art::load_album_art(path).map(|img| art::rasterize(&img, width, height));
    let mut cache = app.art.lock().unwrap();
    cache.key = Some(key);
    cache.raster = raster;
}

struct ArtView<'a> {
    raster: Option<&'a ArtRaster>,
}

impl Widget for ArtView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(raster) = self.raster else {
            let msg = "no artwork";
            if area.width >= msg.len() as u16 && area.height > 0 {
                let x = area.x + (area.width.saturating_sub(msg.len() as u16)) / 2;
                let y = area.y + area.height / 2;
                buf.set_string(
                    x,
                    y,
                    msg,
                    Style::default().fg(Color::DarkGray),
                );
            }
            return;
        };
        let max_x = area.width.min(raster.width);
        let max_y = area.height.min(raster.height);
        let ox = area.x + (area.width.saturating_sub(max_x)) / 2;
        let oy = area.y + (area.height.saturating_sub(max_y)) / 2;
        for y in 0..max_y {
            for x in 0..max_x {
                let Some(cell) = raster.cell(x, y) else {
                    continue;
                };
                let dest = &mut buf[(ox + x, oy + y)];
                dest.set_char(cell.ch);
                dest.set_style(
                    Style::default()
                        .fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                        .bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2)),
                );
            }
        }
    }
}

fn draw_progress(frame: &mut Frame, area: Rect, app: &App) {
    maybe_probe_dest(app);
    let p = app.progress.lock().unwrap().clone();
    let show_dest = p.dest_total > 0;
    let top_h = meter_stack_height(show_dest).min(area.height.saturating_sub(8));
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_h), Constraint::Min(4)])
        .split(area);
    let meters = split[0];
    let body = split[1];
    let art_area = art_panel_rect(body);
    let logs_area = match art_area {
        Some(art) => Rect {
            x: body.x,
            y: body.y,
            width: body.width.saturating_sub(art.width),
            height: body.height,
        },
        None => body,
    };

    let meter_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if show_dest {
            vec![
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(3),
            ]
        } else {
            vec![Constraint::Length(3), Constraint::Min(3)]
        })
        .split(meters);
    let eta = eta_label(&p);
    let spin = spinner_frame(elapsed_ms(&p));
    let mut title_spans = vec![
        Span::styled(
            format!(" {spin} "),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "{}  →  {}  ",
            p.title,
            app.config.device_name()
        )),
    ];
    title_spans.push(Span::styled(
        format!("{} elapsed", walkman::human_elapsed(elapsed_secs(&p))),
        Style::default().fg(Color::Gray),
    ));
    if let Some((text, color)) = &eta {
        title_spans.push(Span::raw("  "));
        title_spans.push(Span::styled(
            text.clone(),
            Style::default().fg(*color).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(title_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(AMBER)),
        ),
        meter_chunks[0],
    );
    let mut gauge_idx = 1;
    if show_dest {
        let current = live_dest_used(&p);
        let projected = p.dest_projected.min(p.dest_total);
        let inner_width = meter_chunks[1].width.saturating_sub(2).max(8) as usize;
        let overflow = p.dest_projected > p.dest_total;
        let mut title = vec![
            Span::raw(" dest  "),
            Span::styled(
                walkman::human_size(current),
                Style::default().fg(DEST_SOLID).add_modifier(Modifier::BOLD),
            ),
        ];
        if p.dest_projected != current {
            title.push(Span::raw("  →  "));
            title.push(Span::styled(
                walkman::human_size(p.dest_projected.min(p.dest_total)),
                Style::default().fg(if overflow { Color::Red } else { DEST_GHOST }),
            ));
            title.push(Span::raw(" after"));
        }
        title.push(Span::raw("  /  "));
        title.push(Span::styled(
            walkman::human_size(p.dest_total),
            Style::default().fg(Color::DarkGray),
        ));
        if overflow {
            title.push(Span::styled(
                "  OVERFLOW",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(
            Paragraph::new(dest_capacity_line(
                current,
                projected,
                p.dest_total,
                inner_width,
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Line::from(title)),
            ),
            meter_chunks[1],
        );
        gauge_idx = 2;
    }
    if p.total == 0 {
        let msg = if p.message.is_empty() {
            "please wait".to_string()
        } else {
            p.message.clone()
        };
        let loader = Line::from(vec![
            Span::styled(
                format!(" {spin}  {msg}  "),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} elapsed", walkman::human_elapsed(elapsed_secs(&p))),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(loader).block(
                Block::default()
                    .title(" Loading ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(AMBER)),
            ),
            meter_chunks[gauge_idx],
        );
    } else {
        let ratio = (p.done as f64 / p.total as f64).clamp(0.0, 1.0);
        let label = meter_label(&p);
        let gauge_title = if p.message.is_empty() {
            Line::from("")
        } else {
            tagged_line(&p.message)
        };
        frame.render_widget(
            Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(gauge_title))
                .gauge_style(Style::default().fg(AMBER).bg(Color::DarkGray))
                .ratio(ratio)
                .label(label),
            meter_chunks[gauge_idx],
        );
    }
    let recent: Vec<Line> = p
        .lines
        .iter()
        .rev()
        .take(16)
        .rev()
        .map(|s| tagged_line(s))
        .collect();
    frame.render_widget(
        Paragraph::new(recent).block(
            Block::default()
                .title(" recent — q quit ")
                .borders(Borders::ALL),
        ),
        logs_area,
    );
    if let Some(art_area) = art_area {
        let block = Block::default()
            .title(" art ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(art_area);
        frame.render_widget(block, art_area);
        let path = current_audio_path(&app.config, &p.message);
        if let Some(path) = path {
            prepare_art(app, &path, inner.width, inner.height);
            let cache = app.art.lock().unwrap();
            frame.render_widget(ArtView { raster: cache.raster.as_ref() }, inner);
        } else {
            frame.render_widget(ArtView { raster: None }, inner);
        }
    }
}

fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn caret_on() -> bool {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis() / 400) % 2 == 0)
        .unwrap_or(true)
}

fn draw_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(8).max(42);
    let pop = popup_area(area, width, 9);
    frame.render_widget(Clear, pop);
    let hidden = app.prompt_kind == "lastfm_password";
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(Block::default().borders(Borders::ALL).inner(pop));
    frame.render_widget(
        Block::default()
            .title(" Type ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(AMBER)),
        pop,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            app.prompt_label.clone(),
            Style::default().fg(Color::Gray),
        ))),
        chunks[0],
    );
    let input = prompt_line(&app.prompt_buf, app.prompt_cursor, hidden, caret_on());
    frame.render_widget(
        Paragraph::new(input).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(" ⏎ save    esc cancel    ←→ move    ^U clear"),
        chunks[2],
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(8).max(42);
    let pop = popup_area(area, width, 9);
    frame.render_widget(Clear, pop);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(Block::default().borders(Borders::ALL).inner(pop));
    frame.render_widget(
        Block::default()
            .title(" Confirm ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(AMBER)),
        pop,
    );
    frame.render_widget(
        Paragraph::new(app.confirm_q.clone()).wrap(Wrap { trim: false }),
        chunks[0],
    );
    let input = prompt_line(&app.confirm_buf, app.confirm_buf.chars().count(), false, caret_on());
    frame.render_widget(
        Paragraph::new(input).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" answer "),
        ),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(" y confirm    n cancel    or type yes/no and ⏎"),
        chunks[2],
    );
}

fn cinder_only(action: &str) -> bool {
    matches!(action, "audiomuse" | "scrobbles")
}

fn action_enabled(config: &AppConfig, action: &str) -> bool {
    !(cinder_only(action) && config.is_android())
}

fn menu_item_style(selected: bool, enabled: bool) -> Style {
    if !enabled {
        let mut style = Style::default()
            .fg(Color::Rgb(88, 88, 88))
            .add_modifier(Modifier::DIM);
        if selected {
            style = style.bg(Color::Rgb(36, 36, 36));
        }
        return style;
    }
    if selected {
        Style::default()
            .fg(Color::Black)
            .bg(AMBER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn cap_text_rows(mut rows: Vec<String>) -> Vec<String> {
    const CAP: usize = 500;
    if rows.is_empty() {
        return vec!["(empty)".into()];
    }
    if rows.len() > CAP {
        let keep = CAP.saturating_sub(1);
        let extra = rows.len() - keep;
        rows.truncate(keep);
        rows.push(format!("… and {extra} more"));
    }
    rows
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, Show);
    }
}

pub fn run() -> Result<()> {
    let mut app = App::new();
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let _restore = RestoreTerminal;
    app.rescan();
    let result = loop {
        app.poll_task();
        terminal.draw(|f| draw(f, &app))?;
        if app.should_quit {
            break Ok(());
        }
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                let quit = key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
                if quit {
                    app.should_quit = true;
                } else if key.kind != KeyEventKind::Release {
                    app.handle_key(key);
                }
            }
        }
        if app.should_quit {
            break Ok(());
        }
    };
    result
}

#[cfg(test)]
mod dest_bar_tests {
    use super::{
        backspace_at, cap_text_rows, cinder_only, commit_eta, dest_bar_segments, displayed_eta_secs,
        eta_label, insert_at, live_dest_used, menu_item_style, meter_label, prompt_line,
        prompt_shown, sample_rate, should_probe_dest, spinner_frame, Progress, ProgressUnit,
    };
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};

    #[test]
    fn dest_used_drops_after_planned_removals() {
        let p = Progress {
            dest_total: 100,
            dest_projected: 60,
            dest_freed: 20,
            probe_used: 80,
            probe_done: 0,
            done: 0,
            unit: ProgressUnit::Count,
            ..Progress::default()
        };
        assert_eq!(live_dest_used(&p), 60);
        let copying = Progress {
            dest_total: 100,
            dest_projected: 70,
            dest_freed: 20,
            probe_used: 80,
            probe_done: 0,
            done: 10,
            unit: ProgressUnit::Bytes,
            ..Progress::default()
        };
        assert_eq!(live_dest_used(&copying), 70);
    }

    #[test]
    fn android_menu_dims_cinder_only_actions() {
        assert!(cinder_only("audiomuse"));
        assert!(cinder_only("scrobbles"));
        assert!(!cinder_only("sync"));
        assert!(!cinder_only("likes"));
        let dim = menu_item_style(false, false);
        assert!(dim.add_modifier.contains(ratatui::style::Modifier::DIM));
        let live = menu_item_style(true, true);
        assert!(live.add_modifier.contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn remote_dest_is_not_probed_while_busy() {
        assert!(should_probe_dest(false, "adb://serial/sdcard/Music"));
        assert!(!should_probe_dest(true, "adb://serial/sdcard/Music"));
        assert!(!should_probe_dest(true, "mtp://WALKMAN/SD/Music"));
        assert!(should_probe_dest(true, r"E:\MUSIC"));
        assert!(!should_probe_dest(false, ""));
    }

    #[test]
    fn cap_text_rows_truncates_with_summary() {
        let rows: Vec<String> = (0..600).map(|i| i.to_string()).collect();
        let capped = cap_text_rows(rows);
        assert_eq!(capped.len(), 500);
        assert!(capped.last().unwrap().contains("more"));
    }

    #[test]
    fn ghost_extends_past_current_when_adding() {
        let (solid, ghost, empty) = dest_bar_segments(10, 40, 100, 10);
        assert_eq!(solid, 1);
        assert_eq!(ghost, 3);
        assert_eq!(empty, 6);
    }

    #[test]
    fn no_ghost_when_current_already_at_projected() {
        let (solid, ghost, empty) = dest_bar_segments(40, 40, 100, 10);
        assert_eq!(solid, 4);
        assert_eq!(ghost, 0);
        assert_eq!(empty, 6);
    }

    fn progress_at(elapsed: Duration, done: u64, total: u64, unit: ProgressUnit) -> Progress {
        let bps = if unit == ProgressUnit::Bytes {
            10.0 * 1024.0 * 1024.0
        } else {
            50.0
        };
        let remaining = total.saturating_sub(done) as f64 / bps;
        Progress {
            done,
            total,
            unit,
            started: Instant::now().checked_sub(elapsed),
            bps: Some(bps),
            ticks: 4,
            eta_secs: Some(remaining),
            eta_at: Some(Instant::now()),
            ..Progress::default()
        }
    }

    #[test]
    fn meter_shows_percent_and_counts_without_times() {
        let p = progress_at(Duration::from_secs(90), 120, 300, ProgressUnit::Count);
        let label = meter_label(&p);
        assert!(label.contains("40%"), "{label}");
        assert!(label.contains("120 / 300"), "{label}");
        assert!(!label.contains("elapsed"), "{label}");
        assert!(!label.contains("left"), "{label}");
    }

    #[test]
    fn meter_shows_sizes_for_byte_copies() {
        let p = progress_at(
            Duration::from_secs(12),
            50 * 1024 * 1024,
            100 * 1024 * 1024,
            ProgressUnit::Bytes,
        );
        let label = meter_label(&p);
        assert!(label.contains("50%"), "{label}");
        assert!(label.contains("MB"), "{label}");
        assert!(!label.contains("elapsed"), "{label}");
        assert!(!label.contains("left"), "{label}");
    }

    #[test]
    fn art_panel_hidden_on_narrow_frame() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 24,
        };
        assert!(super::art_panel_rect(area).is_none());
    }

    #[test]
    fn art_panel_uses_remaining_height_to_stay_square() {
        let body = Rect {
            x: 0,
            y: 9,
            width: 120,
            height: 21,
        };
        let art = super::art_panel_rect(body).expect("wide remaining area shows art");
        assert_eq!(art.height, 21);
        assert_eq!(art.width, 40, "outer square is 2*height - 2");
        assert_eq!(art.x + art.width, 120);
        assert_eq!(art.y, body.y);
        let inner_w = art.width.saturating_sub(2);
        let inner_h = art.height.saturating_sub(2);
        assert_eq!(inner_w, inner_h * 2);
    }

    #[test]
    fn art_panel_shrinks_height_when_width_is_tight() {
        let body = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 30,
        };
        let art = super::art_panel_rect(body).expect("still room for a square");
        assert!(art.width + super::ART_MIN_LOGS <= body.width);
        assert_eq!(art.height, (art.width + 2) / 2);
        assert!(art.height < body.height);
    }

    #[test]
    fn meter_shows_working_before_totals_arrive() {
        let p = Progress {
            started: Instant::now().checked_sub(Duration::from_secs(8)),
            ..Progress::default()
        };
        let label = meter_label(&p);
        assert_eq!(label, "working…");
    }

    #[test]
    fn stall_does_not_collapse_bps() {
        let mut p = Progress {
            done: 8 * 1024 * 1024,
            total: 80 * 1024 * 1024,
            unit: ProgressUnit::Bytes,
            started: Instant::now().checked_sub(Duration::from_secs(5)),
            last_at: Instant::now().checked_sub(Duration::from_secs(1)),
            last_bytes: 8 * 1024 * 1024,
            bps: Some(10.0 * 1024.0 * 1024.0),
            ticks: 4,
            ..Progress::default()
        };
        let before = p.bps;
        sample_rate(&mut p, 8 * 1024 * 1024);
        assert_eq!(p.bps, before);
    }

    #[test]
    fn eta_ticks_down_without_new_samples() {
        let mut p = progress_at(Duration::from_secs(12), 50, 150, ProgressUnit::Count);
        p.eta_secs = Some(50.0);
        p.eta_at = Some(Instant::now());
        let start = displayed_eta_secs(&p).unwrap();
        p.eta_at = Instant::now().checked_sub(Duration::from_secs(3));
        let later = displayed_eta_secs(&p).unwrap();
        assert!(later < start, "later={later} start={start}");
        let (label, _) = eta_label(&p).expect("eta");
        assert!(label.contains("left"), "{label}");
        assert_ne!(label, "50s left");
    }

    #[test]
    fn small_slowdown_does_not_raise_eta() {
        let mut p = Progress {
            done: 50 * 1024 * 1024,
            total: 150 * 1024 * 1024,
            unit: ProgressUnit::Bytes,
            started: Instant::now().checked_sub(Duration::from_secs(10)),
            bps: Some(10.0 * 1024.0 * 1024.0),
            ticks: 4,
            eta_secs: Some(10.0),
            eta_at: Some(Instant::now()),
            ..Progress::default()
        };
        p.bps = Some(8.0 * 1024.0 * 1024.0);
        commit_eta(&mut p);
        let shown = displayed_eta_secs(&p).unwrap();
        assert!(
            shown <= 10.5,
            "small dip raised ETA to {shown}"
        );
    }

    #[test]
    fn insert_and_backspace_keep_typed_text() {
        let (buf, cur) = insert_at("ac", 1, 'b');
        assert_eq!((buf.as_str(), cur), ("abc", 2));
        let (buf, cur) = backspace_at("abc", 2);
        assert_eq!((buf.as_str(), cur), ("ac", 1));
    }

    #[test]
    fn prompt_echoes_plain_text_and_masks_secrets() {
        assert_eq!(prompt_shown("secret", false), "secret");
        assert_eq!(prompt_shown("secret", true), "••••••");
        let line = prompt_line("D:\\Music", 8, false, true);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("D:\\Musi"), "{text}");
    }

    #[test]
    fn spinner_frames_advance() {
        assert_ne!(spinner_frame(0), spinner_frame(80));
        assert_eq!(spinner_frame(0), spinner_frame(800));
    }

    #[test]
    fn prompt_screen_shows_what_was_typed() {
        let mut app = super::App::new();
        app.screen = super::Screen::Prompt;
        app.overlay_parent = super::Screen::Settings;
        app.prompt_label = "Source library".into();
        app.prompt_kind = "source_dir".into();
        app.prompt_buf = "D:\\Music\\Library".into();
        app.prompt_cursor = app.prompt_buf.chars().count();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut body = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                body.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(body.contains("Source library"), "{body}");
        assert!(body.contains("D:\\Music\\Library"), "{body}");
        assert!(body.contains("Type"), "{body}");
    }

    #[test]
    fn loading_screen_shows_spinner_and_title() {
        let mut app = super::App::new();
        app.screen = super::Screen::Loading;
        app.busy = true;
        {
            let mut p = app.progress.lock().unwrap();
            p.title = "Scanning".into();
            p.message = "Detecting Walkman...".into();
            p.started = Some(Instant::now());
        }
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut body = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                body.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(body.contains("Loading"), "{body}");
        assert!(body.contains("Scanning"), "{body}");
        assert!(body.contains("Detecting Walkman"), "{body}");
    }
}
