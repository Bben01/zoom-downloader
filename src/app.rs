use crate::download::{self, Job, Outcome};
use crate::history::{FileRecord, History};
use crate::naming;
use crate::settings::{self, Settings};
use crate::zoom::{Credentials, Meeting, ZoomClient};
use crate::{AppState, AppWindow, LogItem, MeetingItem, Ymd};
use chrono::{Datelike, Days, Local, NaiveDate, Utc};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_LOG_ROWS: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ready,
    OnDisk,
    Queued,
    Downloading,
    Done,
    Trashed,
    Failed,
    Cancelled,
}

impl Status {
    fn key(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::OnDisk => "downloaded",
            Status::Queued => "queued",
            Status::Downloading => "downloading",
            Status::Done => "done",
            Status::Trashed => "deleted",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Status::Ready => "Not downloaded",
            Status::OnDisk => "On disk",
            Status::Queued => "Queued",
            Status::Downloading => "Downloading",
            Status::Done => "Downloaded",
            Status::Trashed => "Saved · trashed",
            Status::Failed => "Failed",
            Status::Cancelled => "Stopped",
        }
    }

    fn is_on_disk(self) -> bool {
        matches!(self, Status::OnDisk | Status::Done | Status::Trashed)
    }
}

#[derive(Clone, Copy)]
enum Pending {
    Download,
    Trash,
}

struct Row {
    meeting: Meeting,
    file_names: Vec<String>,
    selected: bool,
    status: Status,
    detail: String,
    local_path: Option<PathBuf>,
}

type Rows = Arc<Mutex<Vec<Row>>>;

pub struct App {
    ui: Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    rows: Rows,
    client: Mutex<Option<Arc<ZoomClient>>>,
    cancel: RefCell<Option<CancellationToken>>,
    pending: Cell<Pending>,
    timer: Timer,
}

impl App {
    pub fn install(ui: &AppWindow, rt: tokio::runtime::Handle) -> Rc<App> {
        let app = Rc::new(App {
            ui: ui.as_weak(),
            rt,
            rows: Arc::new(Mutex::new(Vec::new())),
            client: Mutex::new(None),
            cancel: RefCell::new(None),
            pending: Cell::new(Pending::Download),
            timer: Timer::default(),
        });

        let state = ui.global::<AppState>();
        state.set_version(env!("CARGO_PKG_VERSION").into());
        state.set_meetings(ModelRc::new(VecModel::<MeetingItem>::default()));
        state.set_logs(ModelRc::new(VecModel::<LogItem>::default()));
        attach_log_sink(ui.as_weak());

        let s = settings::load();
        apply_settings(&state, &s);
        let today = Local::now().date_naive();
        state.set_to_date(to_ymd(today));
        state.set_from_date(to_ymd(today - Days::new(30)));
        state.set_active_preset(30);

        macro_rules! on {
            ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(&app);
                state.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body;
                    }
                });
            }};
        }

        on!(on_fetch, |a| a.fetch());
        on!(on_start, |a| a.start());
        on!(on_confirmed, |a| match a.pending.get() {
            Pending::Download => a.begin_run(),
            Pending::Trash => a.begin_trash(),
        });
        on!(on_trash_selected, |a| a.trash_selected());
        on!(on_stop, |a| a.stop());
        on!(on_toggle, |a, i| a.toggle(i as usize));
        on!(on_select_all, |a, v| a.select_all(v));
        on!(on_preset, |a, d| a.preset(d));
        on!(on_pick_folder, |a| a.pick_folder());
        on!(on_open_folder, |a| a.open_folder());
        on!(on_open_item, |a, i| a.open_item(i as usize));
        on!(on_save_settings, |a| a.save_settings(true));
        on!(on_test_connection, |a| a.test_connection());
        on!(on_clear_logs, |a| a.with_ui(|ui| {
            let st = ui.global::<AppState>();
            if let Some(vm) = st.get_logs().as_any().downcast_ref::<VecModel<LogItem>>() {
                vm.set_vec(Vec::new());
            }
            st.set_unseen_errors(0);
        }));
        on!(on_open_log_file, |a| a.open_log_file());
        on!(on_open_zoom_marketplace, |a| a.open_zoom_marketplace());
        on!(on_page_changed, |a, p| a.with_ui(|ui| {
            if p == 2 {
                ui.global::<AppState>().set_unseen_errors(0);
            }
        }));

        register(&app);
        log::info!("Zoom Downloader {} ready", env!("CARGO_PKG_VERSION"));
        app
    }

    fn with_ui(&self, f: impl FnOnce(&AppWindow)) {
        if let Some(ui) = self.ui.upgrade() {
            f(&ui);
        }
    }

    fn read_settings(&self) -> Settings {
        let Some(ui) = self.ui.upgrade() else { return Settings::default() };
        let st = ui.global::<AppState>();
        Settings {
            account_id: st.get_account_id().trim().to_string(),
            client_id: st.get_client_id().trim().to_string(),
            client_secret: st.get_client_secret().trim().to_string(),
            user_id: st.get_user_id().trim().to_string(),
            output_dir: st.get_output_dir().trim().to_string(),
            concurrency: st.get_concurrency().clamp(1, 8) as u32,
            delete_after: st.get_delete_after(),
            min_delete_age_days: st.get_min_age_days().max(0) as u32,
        }
    }

    fn save_settings(&self, announce: bool) -> Settings {
        let s = self.read_settings();
        let result = settings::save(&s);
        self.with_ui(|ui| {
            let st = ui.global::<AppState>();
            st.set_credentials_missing(!credentials(&s).is_complete());
            match &result {
                Ok(()) if announce => toast(ui, 1, "Settings saved"),
                Err(e) => {
                    log::error!("Saving settings failed: {e:#}");
                    toast(ui, 2, &format!("Could not save settings: {e}"));
                }
                _ => {}
            }
        });
        s
    }

    fn client_for(&self, creds: &Credentials) -> anyhow::Result<Arc<ZoomClient>> {
        let mut guard = self.client.lock().unwrap();
        if let Some(c) = guard.as_ref() {
            if c.credentials() == creds {
                return Ok(c.clone());
            }
        }
        let c = Arc::new(ZoomClient::new(creds.clone())?);
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Returns a client if the credentials are complete, otherwise sends the user to Settings.
    fn ready_client(&self, s: &Settings) -> Option<Arc<ZoomClient>> {
        let creds = credentials(s);
        if !creds.is_complete() {
            self.with_ui(|ui| {
                toast(ui, 2, "Add your Zoom API credentials first");
                ui.global::<AppState>().set_page(1);
            });
            return None;
        }
        match self.client_for(&creds) {
            Ok(c) => Some(c),
            Err(e) => {
                log::error!("{e:#}");
                self.with_ui(|ui| toast(ui, 2, &e.to_string()));
                None
            }
        }
    }

    // ---------------------------------------------------------------- fetch

    fn fetch(&self) {
        let s = self.save_settings(false);
        let Some(client) = self.ready_client(&s) else { return };
        let Some(ui) = self.ui.upgrade() else { return };
        let st = ui.global::<AppState>();
        let (from, to) = (from_ymd(st.get_from_date()), from_ymd(st.get_to_date()));
        let (Some(from), Some(to)) = (from, to) else { return };
        if from > to {
            toast(&ui, 2, "The start date is after the end date");
            return;
        }

        st.set_fetching(true);
        st.set_fetch_status("Connecting to Zoom…".into());
        log::info!("Fetching recordings from {from} to {to}");

        let weak = self.ui.clone();
        let rows = self.rows.clone();
        let out_dir = PathBuf::from(&s.output_dir);
        self.rt.spawn(async move {
            let progress_ui = weak.clone();
            let result = client
                .list_meetings(&s.user_id, from, to, move |a, b| {
                    let msg = format!("Scanning {} – {}…", a.format("%d %b %Y"), b.format("%d %b %Y"));
                    let _ = progress_ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<AppState>().set_fetch_status(msg.into());
                    });
                })
                .await;

            match result {
                Ok(meetings) => {
                    let history = History::load(&out_dir);
                    let names = naming::plan_names(&meetings);
                    let mut new_rows: Vec<Row> = meetings
                        .into_iter()
                        .zip(names)
                        .map(|(m, names)| build_row(m, names, &history, &out_dir))
                        .collect();
                    new_rows.reverse();

                    let files: usize = new_rows.iter().map(|r| r.meeting.files.len()).sum();
                    let bytes: u64 = new_rows.iter().map(|r| r.meeting.total_size()).sum();
                    let count = new_rows.len();
                    log::info!("Found {count} meetings with {files} videos ({})", fmt_bytes(bytes));
                    *rows.lock().unwrap() = new_rows;

                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        let st = ui.global::<AppState>();
                        st.set_fetching(false);
                        st.set_has_fetched(true);
                        refresh_all(&ui, &rows);
                        let msg = match count {
                            0 => "No recordings found in this range".to_string(),
                            1 => "Found 1 meeting".to_string(),
                            n => format!("Found {n} meetings"),
                        };
                        toast(&ui, if count == 0 { 0 } else { 1 }, &msg);
                    });
                }
                Err(e) => {
                    log::error!("{e:#}");
                    let msg = e.to_string();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.global::<AppState>().set_fetching(false);
                        toast(&ui, 2, &msg);
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------ selection

    fn toggle(&self, idx: usize) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let item = {
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(idx) else { return };
            row.selected = !row.selected;
            item_for(row)
        };
        self.with_ui(|ui| {
            set_row(ui, idx, item);
            update_summary(ui, &self.rows);
        });
    }

    fn select_all(&self, value: bool) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let items: Vec<MeetingItem> = {
            let mut rows = self.rows.lock().unwrap();
            rows.iter_mut()
                .map(|r| {
                    r.selected = value;
                    item_for(r)
                })
                .collect()
        };
        self.with_ui(|ui| {
            for (i, item) in items.into_iter().enumerate() {
                set_row(ui, i, item);
            }
            update_summary(ui, &self.rows);
        });
    }

    fn preset(&self, days: i32) {
        let today = Local::now().date_naive();
        let from = if days < 0 {
            NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today)
        } else {
            today - Days::new(days as u64)
        };
        self.with_ui(|ui| {
            let st = ui.global::<AppState>();
            st.set_from_date(to_ymd(from));
            st.set_to_date(to_ymd(today));
            st.set_active_preset(days);
        });
    }

    // ------------------------------------------------------------- download

    fn start(&self) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let s = self.save_settings(false);
        if self.ready_client(&s).is_none() {
            return;
        }

        let (selected, trashable) = {
            let rows = self.rows.lock().unwrap();
            let selected: Vec<&Row> = rows.iter().filter(|r| r.selected && r.status != Status::Trashed).collect();
            let trashable = selected.iter().filter(|r| old_enough(&r.meeting, s.min_delete_age_days)).count();
            (selected.len(), trashable)
        };
        if selected == 0 {
            self.with_ui(|ui| toast(ui, 0, "Select at least one meeting"));
            return;
        }

        if s.delete_after && trashable > 0 {
            let text = format!(
                "{selected} meeting{} will be downloaded. Once every video of a meeting is safely on disk, \
                 {trashable} of them (older than {} days) will be moved to the Zoom trash, where they stay \
                 recoverable for 30 days.",
                if selected == 1 { "" } else { "s" },
                s.min_delete_age_days
            );
            self.ask(Pending::Download, "Download and clean up?", "Start download", text);
            return;
        }
        self.begin_run();
    }

    fn ask(&self, pending: Pending, title: &str, button: &str, text: String) {
        self.pending.set(pending);
        self.with_ui(|ui| {
            let st = ui.global::<AppState>();
            st.set_confirm_title(title.into());
            st.set_confirm_button(button.into());
            st.set_confirm_text(text.into());
            st.set_confirm_visible(true);
        });
    }

    fn begin_run(&self) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let s = self.read_settings();
        let Some(client) = self.ready_client(&s) else { return };
        let out_dir = PathBuf::from(&s.output_dir);
        if s.output_dir.is_empty() {
            self.with_ui(|ui| toast(ui, 2, "Choose a folder to save recordings to"));
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            self.with_ui(|ui| toast(ui, 2, &format!("Cannot use that folder: {e}")));
            return;
        }

        let history = History::load(&out_dir);
        let mut jobs: Vec<Job> = Vec::new();
        let mut per_meeting: HashMap<usize, usize> = HashMap::new();
        let mut changed: Vec<(usize, MeetingItem)> = Vec::new();
        {
            let mut rows = self.rows.lock().unwrap();
            for (i, row) in rows.iter_mut().enumerate() {
                if !row.selected || row.status == Status::Trashed {
                    continue;
                }
                let mut n = 0;
                for (f, name) in row.meeting.files.iter().zip(&row.file_names) {
                    if history.has_file(&f.id) {
                        continue;
                    }
                    jobs.push(Job {
                        meeting: i,
                        file_id: f.id.clone(),
                        url: f.download_url.clone(),
                        size: f.size,
                        dest: out_dir.join(name),
                    });
                    n += 1;
                }
                per_meeting.insert(i, n);
                if n > 0 {
                    row.status = Status::Queued;
                    row.detail.clear();
                } else if row.status != Status::OnDisk && row.status != Status::Done {
                    row.status = Status::OnDisk;
                }
                changed.push((i, item_for(row)));
            }
        }

        let will_trash = s.delete_after
            && per_meeting.iter().any(|(i, n)| {
                *n == 0 && old_enough(&self.rows.lock().unwrap()[*i].meeting, s.min_delete_age_days)
            });
        if jobs.is_empty() && !will_trash {
            self.with_ui(|ui| toast(ui, 0, "Everything selected is already downloaded"));
            return;
        }

        let total_bytes: u64 = jobs.iter().map(|j| j.size).sum();
        log::info!(
            "Starting: {} videos ({}) into {}",
            jobs.len(),
            fmt_bytes(total_bytes),
            out_dir.display()
        );

        let mut meeting_totals: HashMap<usize, u64> = HashMap::new();
        for j in &jobs {
            *meeting_totals.entry(j.meeting).or_default() += j.size;
        }
        let progress = Arc::new(Progress {
            file_bytes: jobs.iter().map(|_| AtomicU64::new(0)).collect(),
            job_meeting: jobs.iter().map(|j| j.meeting).collect(),
            meeting_totals,
            net_bytes: AtomicU64::new(0),
            total_bytes,
            total_files: jobs.len(),
            finished_files: AtomicUsize::new(0),
        });

        let cancel = CancellationToken::new();
        *self.cancel.borrow_mut() = Some(cancel.clone());

        self.with_ui(|ui| {
            for (i, item) in changed {
                set_row(ui, i, item);
            }
            let st = ui.global::<AppState>();
            st.set_running(true);
            st.set_stopping(false);
            st.set_overall_progress(0.0);
            st.set_progress_label(format!("Starting {} videos · {}", jobs.len(), fmt_bytes(total_bytes)).into());
            st.set_speed("".into());
            st.set_eta("".into());
            update_summary(ui, &self.rows);
        });
        self.start_ticker(progress.clone());

        let run = Arc::new(Run {
            client,
            rows: self.rows.clone(),
            ui: self.ui.clone(),
            history: tokio::sync::Mutex::new(history),
            out_dir,
            delete_after: s.delete_after,
            min_age_days: s.min_delete_age_days,
            progress,
            cancel,
            remaining: per_meeting.iter().map(|(i, n)| (*i, AtomicUsize::new(*n))).collect(),
            outcomes: Mutex::new(HashMap::new()),
            downloaded: AtomicUsize::new(0),
            failed_meetings: AtomicUsize::new(0),
            trashed: AtomicUsize::new(0),
        });
        let concurrency = s.concurrency.clamp(1, 8) as usize;
        let empty_meetings: Vec<usize> = per_meeting.iter().filter(|(_, n)| **n == 0).map(|(i, _)| *i).collect();

        self.rt.spawn(async move {
            use futures::StreamExt;
            for i in empty_meetings {
                run.finish_meeting(i).await;
            }
            futures::stream::iter(jobs.into_iter().enumerate())
                .for_each_concurrent(concurrency, |(j, job)| {
                    let run = run.clone();
                    async move { run.run_job(j, job).await }
                })
                .await;
            run.complete();
        });
    }

    fn start_ticker(&self, progress: Arc<Progress>) {
        let weak = self.ui.clone();
        let mut meter = SpeedMeter::default();
        self.timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
            let Some(ui) = weak.upgrade() else { return };
            let st = ui.global::<AppState>();
            let snap = progress.snapshot();
            let speed = meter.update(snap.net);

            st.set_overall_progress(snap.fraction);
            st.set_progress_label(
                format!(
                    "{} of {} videos · {} of {}",
                    snap.finished,
                    progress.total_files,
                    fmt_bytes(snap.done),
                    fmt_bytes(progress.total_bytes)
                )
                .into(),
            );
            st.set_speed(if speed > 0.0 { format!("{}/s", fmt_bytes(speed as u64)) } else { String::new() }.into());
            let remaining = progress.total_bytes.saturating_sub(snap.done);
            st.set_eta(
                if speed > 1.0 && remaining > 0 {
                    format!("{} left", fmt_eta(remaining as f64 / speed))
                } else {
                    String::new()
                }
                .into(),
            );

            let model = st.get_meetings();
            for (&m, &fraction) in &snap.per_meeting {
                if let Some(mut row) = model.row_data(m) {
                    if row.status == "downloading" && (row.progress - fraction).abs() > 0.001 {
                        row.progress = fraction;
                        model.set_row_data(m, row);
                    }
                }
            }
        });
    }

    fn stop(&self) {
        if let Some(c) = self.cancel.borrow().as_ref() {
            log::warn!("Stopping… partial files are kept and will resume next time");
            c.cancel();
            self.with_ui(|ui| ui.global::<AppState>().set_stopping(true));
        }
    }

    /// Called on the UI thread once a run is fully finished.
    fn run_finished(&self) {
        self.timer.stop();
        *self.cancel.borrow_mut() = None;
    }

    // ---------------------------------------------------------- manual trash

    fn trash_selected(&self) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let count = self.rows.lock().unwrap().iter().filter(|r| is_trashable(r)).count();
        if count == 0 {
            return;
        }
        let text = format!(
            "{count} downloaded meeting{} will be moved to the Zoom trash, where they stay recoverable \
             for 30 days. Your local files are not touched.",
            if count == 1 { "" } else { "s" }
        );
        self.ask(Pending::Trash, "Move to Zoom trash?", "Move to trash", text);
    }

    fn begin_trash(&self) {
        if self.cancel.borrow().is_some() {
            return;
        }
        let s = self.save_settings(false);
        let Some(client) = self.ready_client(&s) else { return };
        let targets: Vec<(usize, Meeting)> = {
            let rows = self.rows.lock().unwrap();
            rows.iter().enumerate().filter(|(_, r)| is_trashable(r)).map(|(i, r)| (i, r.meeting.clone())).collect()
        };
        if targets.is_empty() {
            return;
        }

        let total = targets.len();
        log::info!("Moving {total} downloaded meetings to the Zoom trash");
        let cancel = CancellationToken::new();
        *self.cancel.borrow_mut() = Some(cancel.clone());
        self.with_ui(|ui| {
            let st = ui.global::<AppState>();
            st.set_running(true);
            st.set_trashing(true);
            st.set_stopping(false);
            st.set_overall_progress(0.0);
            st.set_progress_label(format!("Moving {total} meetings to the Zoom trash…").into());
            st.set_speed("".into());
            st.set_eta("".into());
        });

        let rows = self.rows.clone();
        let weak = self.ui.clone();
        let out_dir = PathBuf::from(&s.output_dir);
        self.rt.spawn(async move {
            let mut history = History::load(&out_dir);
            let (mut trashed, mut failed) = (0usize, 0usize);
            for (n, (idx, meeting)) in targets.into_iter().enumerate() {
                if cancel.is_cancelled() {
                    break;
                }
                let result = client.trash_meeting_recordings(&meeting.uuid).await;
                let detail = match &result {
                    Ok(()) => {
                        log::info!("Moved to Zoom trash: {} ({})", meeting.topic, local_date(&meeting));
                        trashed += 1;
                        history.record_trashed(&meeting.uuid);
                        if let Err(e) = history.save(&out_dir) {
                            log::warn!("{e:#}");
                        }
                        String::new()
                    }
                    Err(e) => {
                        log::error!("Could not trash {} ({}): {e:#}", meeting.topic, local_date(&meeting));
                        failed += 1;
                        format!("Cloud cleanup failed: {e}")
                    }
                };
                let item = {
                    let mut rows = rows.lock().unwrap();
                    let Some(row) = rows.get_mut(idx) else { continue };
                    if result.is_ok() {
                        row.status = Status::Trashed;
                        row.selected = false;
                    }
                    row.detail = detail;
                    item_for(row)
                };
                let rows = rows.clone();
                let fraction = (n + 1) as f32 / total as f32;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    set_row(&ui, idx, item);
                    update_summary(&ui, &rows);
                    ui.global::<AppState>().set_overall_progress(fraction);
                });
            }

            let mut parts = vec![format!("{trashed} meeting{} moved to Zoom trash", if trashed == 1 { "" } else { "s" })];
            if failed > 0 {
                parts.push(format!("{failed} failed"));
            }
            let summary =
                format!("{} · {}", if cancel.is_cancelled() { "Stopped" } else { "Finished" }, parts.join(" · "));
            log::info!("{summary}");
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let st = ui.global::<AppState>();
                st.set_running(false);
                st.set_trashing(false);
                st.set_stopping(false);
                st.set_progress_label(summary.clone().into());
                APP.with(|a| {
                    if let Some(app) = a.borrow().as_ref().and_then(|w| w.upgrade()) {
                        app.run_finished();
                    }
                });
                toast(&ui, if failed > 0 { 2 } else { 1 }, &summary);
            });
        });
    }

    // ---------------------------------------------------------------- misc

    fn pick_folder(&self) {
        let current = self.read_settings().output_dir;
        let mut dialog = rfd::FileDialog::new().set_title("Choose where to save recordings");
        // Create it first so the picker opens on the same folder as "Open folder".
        if !current.is_empty() && std::fs::create_dir_all(&current).is_ok() {
            dialog = dialog.set_directory(&current);
        }
        if let Some(path) = dialog.pick_folder() {
            self.with_ui(|ui| ui.global::<AppState>().set_output_dir(path.to_string_lossy().as_ref().into()));
            self.save_settings(false);
        }
    }

    fn open_folder(&self) {
        let dir = self.read_settings().output_dir;
        if dir.is_empty() {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir).map_err(anyhow::Error::from).and_then(|_| open::that_detached(&dir).map_err(Into::into)) {
            self.with_ui(|ui| toast(ui, 2, &format!("Cannot open folder: {e}")));
        }
    }

    fn open_item(&self, idx: usize) {
        let path = self.rows.lock().unwrap().get(idx).and_then(|r| r.local_path.clone());
        if let Some(p) = path {
            reveal(&p);
        }
    }

    fn open_log_file(&self) {
        if let Some(p) = settings::log_file() {
            let _ = open::that_detached(p);
        }
    }

    fn open_zoom_marketplace(&self) {
        if let Err(e) = open::that_detached("https://marketplace.zoom.us/develop/create") {
            self.with_ui(|ui| toast(ui, 2, &format!("Cannot open the browser: {e}")));
        }
    }

    fn test_connection(&self) {
        let s = self.read_settings();
        let creds = credentials(&s);
        let Some(ui) = self.ui.upgrade() else { return };
        let st = ui.global::<AppState>();
        if !creds.is_complete() {
            st.set_connection_state(3);
            st.set_connection_message("Fill in all three credentials".into());
            return;
        }
        st.set_connection_state(1);
        let client = match ZoomClient::new(creds) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                st.set_connection_state(3);
                st.set_connection_message(e.to_string().into());
                return;
            }
        };
        let weak = self.ui.clone();
        self.rt.spawn(async move {
            let result = client.verify(&s.user_id).await;
            if let Err(e) = &result {
                log::error!("Connection test failed: {e:#}");
            } else {
                log::info!("Connection test succeeded");
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let st = ui.global::<AppState>();
                match result {
                    Ok(()) => {
                        st.set_connection_state(2);
                        st.set_connection_message("Connected — credentials work".into());
                    }
                    Err(e) => {
                        st.set_connection_state(3);
                        st.set_connection_message(truncate(&e.to_string(), 90).into());
                    }
                }
            });
        });
    }
}

// ------------------------------------------------------------------ run state

struct Progress {
    file_bytes: Vec<AtomicU64>,
    job_meeting: Vec<usize>,
    meeting_totals: HashMap<usize, u64>,
    net_bytes: AtomicU64,
    total_bytes: u64,
    total_files: usize,
    finished_files: AtomicUsize,
}

struct Snapshot {
    done: u64,
    net: u64,
    finished: usize,
    fraction: f32,
    per_meeting: HashMap<usize, f32>,
}

impl Progress {
    fn snapshot(&self) -> Snapshot {
        let mut per: HashMap<usize, u64> = HashMap::new();
        let mut done = 0;
        for (j, b) in self.file_bytes.iter().enumerate() {
            let b = b.load(Ordering::Relaxed);
            done += b;
            *per.entry(self.job_meeting[j]).or_default() += b;
        }
        let finished = self.finished_files.load(Ordering::Relaxed);
        let fraction = if self.total_bytes > 0 {
            done as f32 / self.total_bytes as f32
        } else if self.total_files > 0 {
            finished as f32 / self.total_files as f32
        } else {
            1.0
        };
        let per_meeting = per
            .into_iter()
            .map(|(m, b)| {
                let total = self.meeting_totals.get(&m).copied().unwrap_or(0);
                (m, if total > 0 { (b as f32 / total as f32).min(1.0) } else { 0.0 })
            })
            .collect();
        Snapshot { done, net: self.net_bytes.load(Ordering::Relaxed), finished, fraction: fraction.min(1.0), per_meeting }
    }
}

#[derive(Default)]
struct SpeedMeter {
    last: Option<(Instant, u64)>,
    ema: f64,
}

impl SpeedMeter {
    fn update(&mut self, bytes: u64) -> f64 {
        let now = Instant::now();
        if let Some((t, b)) = self.last {
            let dt = now.duration_since(t).as_secs_f64();
            if dt > 0.0 {
                let inst = bytes.saturating_sub(b) as f64 / dt;
                self.ema = if self.ema == 0.0 { inst } else { self.ema * 0.85 + inst * 0.15 };
            }
        }
        self.last = Some((now, bytes));
        self.ema
    }
}

#[derive(Default)]
struct MeetingOutcome {
    error: Option<String>,
    cancelled: bool,
    downloaded: usize,
}

struct Run {
    client: Arc<ZoomClient>,
    rows: Rows,
    ui: Weak<AppWindow>,
    history: tokio::sync::Mutex<History>,
    out_dir: PathBuf,
    delete_after: bool,
    min_age_days: u32,
    progress: Arc<Progress>,
    cancel: CancellationToken,
    remaining: HashMap<usize, AtomicUsize>,
    outcomes: Mutex<HashMap<usize, MeetingOutcome>>,
    downloaded: AtomicUsize,
    failed_meetings: AtomicUsize,
    trashed: AtomicUsize,
}

impl Run {
    fn set_status(&self, idx: usize, status: Status, detail: Option<String>) {
        let item = {
            let mut rows = self.rows.lock().unwrap();
            let row = &mut rows[idx];
            row.status = status;
            if let Some(d) = detail {
                row.detail = d;
            }
            if matches!(status, Status::Done | Status::Trashed | Status::OnDisk) {
                row.selected = false;
            }
            item_for(row)
        };
        let rows = self.rows.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            set_row(&ui, idx, item);
            update_summary(&ui, &rows);
        });
    }

    async fn run_job(&self, j: usize, job: Job) {
        let name = job.dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let outcome = if self.cancel.is_cancelled() {
            Outcome::Cancelled
        } else {
            let is_first = self.rows.lock().unwrap()[job.meeting].status == Status::Queued;
            if is_first {
                self.set_status(job.meeting, Status::Downloading, Some(String::new()));
            }
            download::download(
                &self.client,
                &job,
                &self.progress.file_bytes[j],
                &self.progress.net_bytes,
                &self.cancel,
            )
            .await
        };

        match &outcome {
            Outcome::Downloaded | Outcome::AlreadyPresent => {
                self.progress.finished_files.fetch_add(1, Ordering::Relaxed);
                if matches!(outcome, Outcome::Downloaded) {
                    self.downloaded.fetch_add(1, Ordering::Relaxed);
                    log::info!("Saved {name}");
                } else {
                    log::info!("Already on disk: {name}");
                }
                let (uuid, topic) = {
                    let mut rows = self.rows.lock().unwrap();
                    let row = &mut rows[job.meeting];
                    if row.local_path.is_none() {
                        row.local_path = Some(job.dest.clone());
                    }
                    (row.meeting.uuid.clone(), row.meeting.topic.clone())
                };
                let mut history = self.history.lock().await;
                history.record_file(
                    &job.file_id,
                    FileRecord { meeting_uuid: uuid, topic, file_name: name.clone(), size: job.size, downloaded_at: Utc::now() },
                );
                if let Err(e) = history.save(&self.out_dir) {
                    log::warn!("{e:#}");
                }
                self.outcomes.lock().unwrap().entry(job.meeting).or_default().downloaded += 1;
            }
            Outcome::Failed(msg) => {
                self.progress.finished_files.fetch_add(1, Ordering::Relaxed);
                log::error!("{name}: {msg}");
                self.outcomes.lock().unwrap().entry(job.meeting).or_default().error = Some(msg.clone());
            }
            Outcome::Cancelled => {
                self.outcomes.lock().unwrap().entry(job.meeting).or_default().cancelled = true;
            }
        }

        if self.remaining[&job.meeting].fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish_meeting(job.meeting).await;
        }
    }

    async fn finish_meeting(&self, idx: usize) {
        let outcome = self.outcomes.lock().unwrap().remove(&idx).unwrap_or_default();
        let meeting = self.rows.lock().unwrap()[idx].meeting.clone();

        if let Some(err) = outcome.error {
            self.failed_meetings.fetch_add(1, Ordering::Relaxed);
            self.set_status(idx, Status::Failed, Some(err));
            return;
        }
        if outcome.cancelled {
            self.set_status(idx, Status::Cancelled, Some("Partial files kept for resume".into()));
            return;
        }

        let status = if outcome.downloaded > 0 { Status::Done } else { Status::OnDisk };
        if !self.delete_after || self.cancel.is_cancelled() {
            self.set_status(idx, status, Some(String::new()));
            return;
        }
        if !old_enough(&meeting, self.min_age_days) {
            self.set_status(idx, status, Some(format!("Kept in cloud (newer than {} days)", self.min_age_days)));
            return;
        }

        match self.client.trash_meeting_recordings(&meeting.uuid).await {
            Ok(()) => {
                log::info!("Moved to Zoom trash: {} ({})", meeting.topic, local_date(&meeting));
                self.trashed.fetch_add(1, Ordering::Relaxed);
                let mut history = self.history.lock().await;
                history.record_trashed(&meeting.uuid);
                if let Err(e) = history.save(&self.out_dir) {
                    log::warn!("{e:#}");
                }
                drop(history);
                self.set_status(idx, Status::Trashed, Some(String::new()));
            }
            Err(e) => {
                log::error!("Could not trash {} ({}): {e:#}", meeting.topic, local_date(&meeting));
                self.set_status(idx, status, Some(format!("Cloud cleanup failed: {e}")));
            }
        }
    }

    fn complete(&self) {
        let cancelled = self.cancel.is_cancelled();
        let downloaded = self.downloaded.load(Ordering::Relaxed);
        let failed = self.failed_meetings.load(Ordering::Relaxed);
        let trashed = self.trashed.load(Ordering::Relaxed);
        let snap = self.progress.snapshot();

        let mut parts = vec![format!("{downloaded} video{} downloaded", if downloaded == 1 { "" } else { "s" })];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if trashed > 0 {
            parts.push(format!("{trashed} moved to Zoom trash"));
        }
        let summary = format!("{} · {}", if cancelled { "Stopped" } else { "Finished" }, parts.join(" · "));
        if failed > 0 {
            log::warn!("{summary}");
        } else {
            log::info!("{summary}");
        }

        let rows = self.rows.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_running(false);
            st.set_stopping(false);
            st.set_overall_progress(snap.fraction);
            st.set_progress_label(summary.clone().into());
            st.set_speed("".into());
            st.set_eta("".into());

            // Anything still queued never started because the run was stopped.
            let pending: Vec<(usize, MeetingItem)> = {
                let mut rows = rows.lock().unwrap();
                rows.iter_mut()
                    .enumerate()
                    .filter(|(_, r)| matches!(r.status, Status::Queued | Status::Downloading))
                    .map(|(i, r)| {
                        r.status = Status::Cancelled;
                        (i, item_for(r))
                    })
                    .collect()
            };
            for (i, item) in pending {
                set_row(&ui, i, item);
            }
            update_summary(&ui, &rows);
            APP.with(|a| {
                if let Some(app) = a.borrow().as_ref().and_then(|w| w.upgrade()) {
                    app.run_finished();
                }
            });
            toast(&ui, if failed > 0 { 2 } else { 1 }, &summary);
        });
    }
}

thread_local! {
    static APP: RefCell<Option<std::rc::Weak<App>>> = const { RefCell::new(None) };
}

pub fn register(app: &Rc<App>) {
    APP.with(|a| *a.borrow_mut() = Some(Rc::downgrade(app)));
}

// ------------------------------------------------------------------ UI helpers

fn apply_settings(st: &AppState, s: &Settings) {
    st.set_account_id(s.account_id.as_str().into());
    st.set_client_id(s.client_id.as_str().into());
    st.set_client_secret(s.client_secret.as_str().into());
    st.set_user_id(if s.user_id.is_empty() { "me".into() } else { s.user_id.as_str().into() });
    st.set_output_dir(s.output_dir.as_str().into());
    st.set_concurrency(s.concurrency as i32);
    st.set_delete_after(s.delete_after);
    st.set_min_age_days(s.min_delete_age_days as i32);
    st.set_credentials_missing(!credentials(s).is_complete());
}

fn credentials(s: &Settings) -> Credentials {
    Credentials {
        account_id: s.account_id.clone(),
        client_id: s.client_id.clone(),
        client_secret: s.client_secret.clone(),
    }
}

fn build_row(meeting: Meeting, file_names: Vec<String>, history: &History, out_dir: &Path) -> Row {
    let mut local_path = None;
    let mut on_disk = 0;
    for (f, name) in meeting.files.iter().zip(&file_names) {
        let path = out_dir.join(name);
        let exists = std::fs::metadata(&path).map(|m| f.size == 0 || m.len() == f.size).unwrap_or(false);
        if exists && local_path.is_none() {
            local_path = Some(path);
        }
        if exists || history.has_file(&f.id) {
            on_disk += 1;
        }
    }
    let done = on_disk == meeting.files.len();
    Row {
        meeting,
        file_names,
        selected: !done,
        status: if done { Status::OnDisk } else { Status::Ready },
        detail: String::new(),
        local_path,
    }
}

fn item_for(row: &Row) -> MeetingItem {
    let local = row.meeting.start_time.with_timezone(&Local);
    MeetingItem {
        selected: row.selected,
        topic: row.meeting.topic.as_str().into(),
        date: local.format("%a %d %b %Y").to_string().into(),
        time: local.format("%H:%M").to_string().into(),
        duration: fmt_minutes(row.meeting.duration_min).into(),
        files: row.meeting.files.len() as i32,
        size: fmt_bytes(row.meeting.total_size()).into(),
        status: row.status.key().into(),
        status_label: row.status.label().into(),
        progress: if row.status.is_on_disk() { 1.0 } else { 0.0 },
        detail: row.detail.as_str().into(),
        can_open: row.local_path.is_some(),
    }
}

fn set_row(ui: &AppWindow, idx: usize, mut item: MeetingItem) {
    let model = ui.global::<AppState>().get_meetings();
    if idx >= model.row_count() {
        return;
    }
    if item.status == "downloading" {
        if let Some(old) = model.row_data(idx) {
            item.progress = old.progress;
        }
    }
    model.set_row_data(idx, item);
}

fn refresh_all(ui: &AppWindow, rows: &Rows) {
    let items: Vec<MeetingItem> = rows.lock().unwrap().iter().map(item_for).collect();
    ui.global::<AppState>().set_meetings(ModelRc::new(VecModel::from(items)));
    update_summary(ui, rows);
}

fn update_summary(ui: &AppWindow, rows: &Rows) {
    let rows = rows.lock().unwrap();
    let st = ui.global::<AppState>();
    let selected: Vec<&Row> = rows.iter().filter(|r| r.selected).collect();
    let total_size: u64 = rows.iter().map(|r| r.meeting.total_size()).sum();
    let on_disk = rows.iter().filter(|r| r.status.is_on_disk()).count();

    st.set_selected_count(selected.len() as i32);
    st.set_selected_size(fmt_bytes(selected.iter().map(|r| r.meeting.total_size()).sum()).into());
    st.set_all_selected(!rows.is_empty() && selected.len() == rows.len());
    st.set_done_count(on_disk as i32);
    st.set_failed_count(rows.iter().filter(|r| r.status == Status::Failed).count() as i32);
    st.set_deleted_count(rows.iter().filter(|r| r.status == Status::Trashed).count() as i32);
    st.set_trashable_count(rows.iter().filter(|r| is_trashable(r)).count() as i32);
    st.set_list_summary(
        if rows.is_empty() {
            "Choose a date range and fetch your cloud recordings.".to_string()
        } else {
            format!("{} meetings · {} in the cloud · {} already on disk", rows.len(), fmt_bytes(total_size), on_disk)
        }
        .into(),
    );
}

fn toast(ui: &AppWindow, kind: i32, text: &str) {
    let st = ui.global::<AppState>();
    let text: SharedString = text.into();
    st.set_toast_text(text.clone());
    st.set_toast_kind(kind);
    st.set_toast_visible(true);
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(4200), move || {
        if let Some(ui) = weak.upgrade() {
            let st = ui.global::<AppState>();
            if st.get_toast_text() == text {
                st.set_toast_visible(false);
            }
        }
    });
}

fn attach_log_sink(weak: Weak<AppWindow>) {
    crate::logger::set_sink(Box::new(move |level, message| {
        let level = match level {
            log::Level::Error => 2,
            log::Level::Warn => 1,
            _ => 0,
        };
        let item = LogItem {
            time: Local::now().format("%H:%M:%S").to_string().into(),
            level,
            message: message.into(),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            let model = st.get_logs();
            if let Some(vm) = model.as_any().downcast_ref::<VecModel<LogItem>>() {
                vm.insert(0, item);
                if vm.row_count() > MAX_LOG_ROWS {
                    vm.remove(vm.row_count() - 1);
                }
            }
            if level == 2 && st.get_page() != 2 {
                st.set_unseen_errors(st.get_unseen_errors() + 1);
            }
        });
    }));
}

fn reveal(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg("/select,").arg(path).spawn().map(|_| ());
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg("-R").arg(path).spawn().map(|_| ());
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = open::that_detached(path.parent().unwrap_or(path));
    if let Err(e) = result {
        log::warn!("Could not open file location: {e}");
    }
}

fn is_trashable(r: &Row) -> bool {
    r.selected && matches!(r.status, Status::OnDisk | Status::Done)
}

fn old_enough(m: &Meeting, min_days: u32) -> bool {
    let date = m.start_time.with_timezone(&Local).date_naive();
    (Local::now().date_naive() - date).num_days() >= min_days as i64
}

fn local_date(m: &Meeting) -> String {
    m.start_time.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn to_ymd(d: NaiveDate) -> Ymd {
    Ymd { year: d.year(), month: d.month() as i32, day: d.day() as i32 }
}

fn from_ymd(d: Ymd) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(d.year, d.month as u32, d.day as u32)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn fmt_minutes(min: u32) -> String {
    match min {
        0 => "< 1 min".into(),
        m if m < 60 => format!("{m} min"),
        m => format!("{}h {:02}m", m / 60, m % 60),
    }
}

fn fmt_eta(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(112_128_303), "107 MB");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_minutes(75), "1h 15m");
        assert_eq!(fmt_eta(3725.0), "1h 02m");
    }
}
