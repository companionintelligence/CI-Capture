use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::sync::mpsc::{
    Receiver, Sender, UnboundedReceiver, UnboundedSender, channel, unbounded_channel,
};
use tokio::time::{self, Interval, MissedTickBehavior};
use uuid::Uuid;
use vizier::diff::create_diff_envelope;
use vizier::observation::Observation;
use vizier::observer::{Observer, ObserverConfig, create_observer};

use crate::analysis::{AnalysisResult, VisionAnalyzer};
use crate::bluebubbles;
use crate::capture::{CapturedImage, capture_primary, diff_percent, save_to_path};
use crate::config::AppConfig;
#[cfg(feature = "legacy-postgres")]
use crate::db::PostgresRecorder;
#[cfg(feature = "legacy-minio")]
use crate::db::ScreenshotRecord;
use crate::db::VizierEventRecord;
use crate::file_tracker;
use crate::imessage;
use crate::knowledge;
#[cfg(feature = "legacy-minio")]
use crate::minio::MinioUploader;
use crate::server_ingest;

const MIN_INTERVAL_SECS: u64 = 30;
const ANALYSIS_QUEUE_CAPACITY: usize = 2;
/// Largest vizier payload we'll attempt to POST. Sits under CI-Server's 10 MB
/// ingest body limit so an oversized snapshot is skipped, not bounced as 413.
const MAX_VIZIER_PAYLOAD_BYTES: usize = 9 * 1024 * 1024;

/// Set once the worker has logged that vizier observations are coming back
/// empty; later empty ticks log at debug so a Windows/Linux install does not
/// fill its log with the same warning every 30 s.
static VIZIER_EMPTY_WARNED: AtomicBool = AtomicBool::new(false);

/// How many vizier ticks have been skipped as empty since launch. Read by
/// [`vizier_empty_ticks_skipped`]; the P2 health row is the intended consumer
/// so an idle lane can be told apart from a broken observer.
static VIZIER_EMPTY_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Number of vizier ticks skipped because the observation was empty and the
/// server already held an empty state (see [`vizier_should_skip_send`]).
#[cfg_attr(not(test), allow(dead_code))] // consumer is the P2 health row
pub(crate) fn vizier_empty_ticks_skipped() -> u64 {
    VIZIER_EMPTY_SKIPPED.load(Ordering::Relaxed)
}

/// True when a vizier observation carries nothing worth shipping: no focused
/// window, no window list, no filesystem events, and no network connections.
///
/// Placeholder fields the baseline observer always fills (timestamps, a
/// zero-sized display, the observing process's own cwd/shell) do not count —
/// they are the same every tick and say nothing about what the person is
/// doing. Kept as a pure function so the skip rule is testable on every OS,
/// independent of which native observer (if any) is compiled in.
pub(crate) fn vizier_observation_is_empty(observation: &Observation) -> bool {
    observation.focus.is_none()
        && observation.windows.is_empty()
        && observation.fs_events.is_empty()
        && observation.net_connections.is_empty()
}

/// Whether this tick's vizier POST can be skipped.
///
/// Skipping is only safe when the observation is empty **and** the last state
/// the server holds (`previous`, the baseline the next diff would be built
/// against) is empty too — or there is no baseline at all, which is the
/// Windows / non-Hyprland-Linux steady state from the first tick. If the
/// previous send carried focus, windows, fs events or connections, this
/// empty tick is shipped once so the server's "current" state is cleared:
/// the last file edit stops being reported as still in progress, an
/// all-windows-closed transition is recorded, and `ts`/`monotonic_ms`
/// advance. Only after that does the lane go quiet.
pub(crate) fn vizier_should_skip_send(
    previous: Option<&Observation>,
    current: &Observation,
) -> bool {
    vizier_observation_is_empty(current) && previous.is_none_or(vizier_observation_is_empty)
}

#[derive(Clone)]
pub struct WorkerHandle {
    tx: UnboundedSender<WorkerCommand>,
    running: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub fn spawn(initial_config: AppConfig) -> Self {
        let (tx, rx) = unbounded_channel();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to create runtime");
            runtime.block_on(async move {
                if let Err(err) = run_worker(rx, initial_config, worker_running).await {
                    log::error!("worker terminated: {err:?}");
                }
            });
        });

        Self { tx, running }
    }

    pub fn update_config(&self, config: AppConfig) {
        let _ = self.tx.send(WorkerCommand::UpdateConfig(Box::new(config)));
    }

    pub fn pause(&self) {
        let _ = self.tx.send(WorkerCommand::Pause);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(WorkerCommand::Resume);
    }

    pub fn capture_now(&self) {
        let _ = self.tx.send(WorkerCommand::CaptureNow);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(WorkerCommand::Shutdown);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

enum WorkerCommand {
    UpdateConfig(Box<AppConfig>),
    Pause,
    Resume,
    CaptureNow,
    Shutdown,
}

/// What the analysis loop's storage sweep needs to know, fed over a watch
/// channel: the current config plus whether capture is active (paused users
/// must not have backlog uploads running behind their back).
#[derive(Clone)]
struct SweepContext {
    config: AppConfig,
    active: bool,
}

struct PendingCapture {
    image: CapturedImage,
    filename: String,
    // Only read by the legacy MinIO path.
    #[cfg_attr(not(feature = "legacy-minio"), allow(dead_code))]
    timestamp: chrono::DateTime<Utc>,
    config: AppConfig,
}

async fn run_worker(
    mut commands: UnboundedReceiver<WorkerCommand>,
    initial_config: AppConfig,
    running_flag: Arc<AtomicBool>,
) -> Result<()> {
    let (analysis_tx, mut analysis_rx) = channel::<PendingCapture>(ANALYSIS_QUEUE_CAPACITY);
    // Live config + pause feed for the analysis loop's storage sweep, so
    // retention and backlog retry keep working between captures and across
    // reconfigs — and uploads stop while the user has captures paused.
    let (config_watch_tx, config_watch_rx) = tokio::sync::watch::channel(SweepContext {
        config: initial_config.clone(),
        active: true,
    });

    tokio::spawn(async move {
        run_analysis_loop(&mut analysis_rx, config_watch_rx).await;
    });

    let mut state = WorkerState::new(initial_config, running_flag, analysis_tx, config_watch_tx);
    state.restart_file_tracker().await;
    state.restart_bluebubbles().await;
    state.restart_imessage().await;
    state.restart_knowledge().await;
    let mut interval = build_interval(state.config.capture.screenshot_interval_secs);

    loop {
        tokio::select! {
            Some(cmd) = commands.recv() => {
                if state.handle_command(cmd, &mut interval).await? {
                    break;
                }
            }
            _ = interval.tick() => {
                if !state.is_active() {
                    continue;
                }
                // Minted once per tick and threaded into both captures below,
                // so CI-Server can correlate the screenshot with the
                // desktop-activity event taken in the same tick — the two
                // otherwise carry no shared identifier (they upload via
                // completely separate, independently-retried paths). See
                // CI-Server's ci.events.capture_tick_id / ci.files.capture_tick_id
                // (migration 20260830042159_add_capture_correlation_to_files_and_events).
                let capture_tick_id = Uuid::new_v4();
                if let Err(err) = state.capture_once(capture_tick_id).await {
                    log::error!("screenshot worker error: {err:?}");
                }
                // Activity (vizier) capture runs every active tick, independent
                // of whether the screenshot changed — a static screen (reading,
                // a call) still has focus/window/fs activity worth timelining,
                // and vizier's own diff keeps the sent payload small. Still
                // suppressed while the screen is locked (capture_once sets
                // capture_blocked), so we don't timeline the lock screen.
                if state.config.vizier.enabled
                    && !state.capture_blocked
                    && let Err(err) = state.capture_vizier(capture_tick_id).await
                {
                    log::error!("vizier worker error: {err:?}");
                }
            }
        }
    }

    Ok(())
}

/// How often the analysis loop sweeps local stores for retention pruning
/// and backlog re-upload.
const STORE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

async fn run_analysis_loop(
    rx: &mut Receiver<PendingCapture>,
    mut config_watch: tokio::sync::watch::Receiver<SweepContext>,
) {
    let analyzer = VisionAnalyzer::default();
    let mut sweep_tick = time::interval(STORE_SWEEP_INTERVAL);
    sweep_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut was_configured = server_ingest::is_configured(&config_watch.borrow().config);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(capture) = maybe else { break };
                handle_fresh_capture(&analyzer, &capture).await;
            }
            _ = sweep_tick.tick() => {
                let ctx = config_watch.borrow().clone();
                sweep_capture_store(&analyzer, &ctx, false).await;
                sweep_activity_store(&ctx, false).await;
            }
            Ok(()) = config_watch.changed() => {
                let ctx = config_watch.borrow().clone();
                let now_configured = server_ingest::is_configured(&ctx.config);
                if now_configured && !was_configured {
                    log::info!("Memory connected; draining local capture backlog");
                    sweep_capture_store(&analyzer, &ctx, true).await;
                    sweep_activity_store(&ctx, true).await;
                }
                was_configured = now_configured;
            }
        }
    }
}

async fn handle_fresh_capture(analyzer: &VisionAnalyzer, capture: &PendingCapture) {
    let png_bytes = match capture.image.to_png_bytes() {
        Ok(b) => b,
        Err(e) => {
            log::error!("failed to encode png for analysis: {e:?}");
            return;
        }
    };

    if server_ingest::is_configured(&capture.config) {
        ingest_capture_to_server(analyzer, &capture.config, &capture.filename, &png_bytes).await;
        return;
    }

    // No CI-Server: fall back to the legacy local path (still needs the
    // local analysis result).
    let analysis = match analyzer.analyze(&capture.config.analysis, &png_bytes).await {
        Ok(result) => result,
        Err(err) => {
            log::warn!("vision analysis failed: {err:?}");
            AnalysisResult::default()
        }
    };
    #[cfg(feature = "legacy-minio")]
    if let Err(err) = record_capture_legacy(capture, &png_bytes, &analysis).await {
        log::error!("failed to write legacy capture rows: {err:?}");
    }
    #[cfg(not(feature = "legacy-minio"))]
    {
        let _ = &analysis;
        log::warn!(
            "no CI-Server token configured; screenshot kept on disk under retention (rebuild with --features legacy-minio for the MinIO fallback)"
        );
    }
}

/// One pass over the local screenshot store: prune per the retention policy,
/// then re-upload the oldest still-unsent frames. A frame on disk is by
/// definition unacked — successful ingest deletes it — so this is what makes
/// the screenshot lane durable across outages, auth expiry, dropped analysis
/// slots, and restarts.
async fn sweep_capture_store(analyzer: &VisionAnalyzer, ctx: &SweepContext, force: bool) {
    let config = &ctx.config;
    let storage_dir = config.capture.storage_dir.clone();
    let entries = tokio::task::spawn_blocking(move || crate::capture_store::scan(&storage_dir))
        .await
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }

    let now = std::time::SystemTime::now();
    let plan = if force {
        crate::capture_store::plan_reconnect_sweep(entries, &config.capture, now)
    } else {
        crate::capture_store::plan_sweep(entries, &config.capture, now)
    };

    for path in &plan.prune {
        match std::fs::remove_file(path) {
            Ok(()) => log::info!("retention: pruned {}", path.display()),
            Err(err) => log::warn!("retention: failed to prune {}: {err}", path.display()),
        }
    }

    // Retention (local disk hygiene) always runs; uploads stop when the user
    // paused captures or no server is configured.
    if !ctx.active || !server_ingest::is_configured(config) {
        return;
    }

    for path in &plan.retry {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let read_path = path.clone();
        let png_bytes = match tokio::task::spawn_blocking(move || std::fs::read(read_path)).await {
            Ok(Ok(b)) => b,
            Ok(Err(err)) => {
                log::warn!("backlog retry: cannot read {}: {err}", path.display());
                continue;
            }
            Err(err) => {
                log::warn!("backlog retry: read task failed: {err}");
                continue;
            }
        };
        log::info!("backlog retry: re-uploading {filename}");
        // Same path as a fresh capture — deletes the file on server ack.
        ingest_capture_to_server(analyzer, config, &filename, &png_bytes).await;
    }
}

async fn sweep_activity_store(ctx: &SweepContext, force: bool) {
    let config = &ctx.config;
    let dir = crate::capture_store::activity_dir(&config.capture.storage_dir);
    let scan_dir = dir.clone();
    let entries =
        tokio::task::spawn_blocking(move || crate::capture_store::scan_activity(&scan_dir))
            .await
            .unwrap_or_default();
    if entries.is_empty() {
        return;
    }

    let now = std::time::SystemTime::now();
    let plan = if force {
        crate::capture_store::plan_reconnect_sweep(entries, &config.capture, now)
    } else {
        crate::capture_store::plan_sweep(entries, &config.capture, now)
    };

    for path in &plan.prune {
        match std::fs::remove_file(path) {
            Ok(()) => log::info!("retention: pruned {}", path.display()),
            Err(err) => log::warn!("retention: failed to prune {}: {err}", path.display()),
        }
    }

    if !ctx.active || !server_ingest::is_configured(config) {
        return;
    }

    for path in &plan.retry {
        let read_path = path.clone();
        let bytes = match tokio::task::spawn_blocking(move || std::fs::read(read_path)).await {
            Ok(Ok(b)) => b,
            Ok(Err(err)) => {
                log::warn!("activity retry: cannot read {}: {err}", path.display());
                continue;
            }
            Err(err) => {
                log::warn!("activity retry: read task failed: {err}");
                continue;
            }
        };
        let event: VizierEventRecord = match serde_json::from_slice(&bytes) {
            Ok(e) => e,
            Err(err) => {
                log::warn!("activity retry: bad payload {}: {err}", path.display());
                continue;
            }
        };
        log::info!("activity retry: re-sending {}", path.display());
        match server_ingest::ingest_vizier_event(config, event.device_id, &event).await {
            Ok(()) => {
                if let Err(err) = std::fs::remove_file(path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("failed to remove acked activity {}: {err}", path.display());
                }
            }
            Err(err) => log::warn!("activity retry failed (will keep on disk): {err:?}"),
        }
    }
}

fn persist_vizier_event(config: &AppConfig, event: &VizierEventRecord) -> Result<()> {
    let dir = crate::capture_store::activity_dir(&config.capture.storage_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating activity dir {}", dir.display()))?;
    let slug = event.observed_at.format("%Y%m%dT%H%M%S");
    let path = dir.join(format!(
        "vizier_{}_{}_{}.json",
        slug,
        event.monotonic_ms,
        Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec(event).context("serializing vizier event")?;
    std::fs::write(&path, bytes)
        .with_context(|| format!("writing activity event {}", path.display()))?;
    Ok(())
}

/// Upload a capture to CI-Server. When the applet has its own vision endpoint
/// configured, it analyses the screenshot locally and writes the result onto
/// the file as title/description (CI-Server skips its own Ollama analysis).
/// Otherwise it asks CI-Server to analyse the image itself (Ollama fallback).
async fn ingest_capture_to_server(
    analyzer: &VisionAnalyzer,
    config: &AppConfig,
    filename: &str,
    png_bytes: &[u8],
) {
    let has_endpoint = !config.analysis.endpoint.trim().is_empty();

    // Analyse locally FIRST (when an endpoint is configured). Only then do we
    // know whether we hold a description to write — so we only tell the server
    // to skip its own Ollama description when we can actually replace it.
    // Otherwise a failed/unreachable local endpoint would leave a permanent
    // zero-metadata frame (the server was told to skip, and we have nothing).
    let local_analysis = if has_endpoint {
        match analyzer.analyze(&config.analysis, png_bytes).await {
            Ok(result) => Some(result),
            Err(err) => {
                log::warn!(
                    "local vision analysis failed; deferring to server-side description: {err:?}"
                );
                None
            }
        }
    } else {
        None
    };
    let analysed_locally = local_analysis.is_some();

    // `skipDescription=true` suppresses CI-Server's own Ollama description pass.
    // Suppress ONLY when we have a local result to PATCH in below; otherwise the
    // server describes the capture, which is the supported default.
    //
    // `inventory` is always false. It does NOT request the description — that
    // was true before CI-Server #1615 and this call site was never updated. What
    // it actually triggers is CI-Server's InventoryListener: a *second* vision
    // pass over the full-resolution original running an "identify objects and
    // estimate their USD value" prompt, writing ci.inventory rows. On screen
    // captures that is meaningless output bought with roughly double the GPU
    // time on the user's Companion Memory server.
    let file_id = match server_ingest::ingest_capture(
        config,
        filename,
        png_bytes,
        false,
        analysed_locally,
        None,
    )
    .await
    {
        Ok(id) => id,
        Err(err) => {
            // The PNG stays in storage_dir; the periodic store sweep will
            // retry it, so a failed upload is deferred — not lost.
            log::error!(
                "failed to ingest screenshot (will retry from disk): {err:?} (file={filename}, size={:.1} KB)",
                png_bytes.len() as f64 / 1024.0
            );
            return;
        }
    };

    // When we analysed locally, CI-Server was told to skip its own
    // description, so we MUST land ours before discarding the local copy —
    // otherwise a failed metadata PATCH would orphan a description-less,
    // recall-invisible frame with no local copy to retry from. If the PATCH
    // fails we keep the PNG: the store sweep re-uploads it, and CI-Server's
    // content-hash dedup returns the SAME file id, so the retry repairs this
    // very frame rather than duplicating it.
    if let Some(analysis) = &local_analysis {
        let title = analysis
            .primary_application
            .clone()
            .or_else(|| analysis.activity_category.clone())
            .unwrap_or_else(|| "Screen capture".to_string());
        let description = analysis.activity_summary.clone();

        if let Err(err) =
            server_ingest::set_file_metadata(config, &file_id, Some(&title), description.as_deref())
                .await
        {
            log::warn!(
                "failed to set file metadata for {file_id}; keeping local copy for retry: {err:?}"
            );
            return; // leave the PNG on disk for the sweep to re-drive
        }
    }

    // The description is durable on the server (or CI-Server is describing the
    // frame itself) — the local copy's job is done. Removing it keeps
    // storage_dir from becoming an unbounded plaintext archive of the screen.
    let local_path = config.capture.storage_dir.join(filename);
    if let Err(err) = std::fs::remove_file(&local_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "failed to remove acked screenshot {}: {err}",
            local_path.display()
        );
    }
}

fn build_interval(seconds: u64) -> Interval {
    let secs = seconds.max(MIN_INTERVAL_SECS);
    let mut interval = time::interval(Duration::from_secs(secs));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

struct WorkerState {
    config: AppConfig,
    paused: bool,
    capture_blocked: bool,
    /// True once the desktop has sat idle (no keyboard/mouse input) for at
    /// least a full screenshot interval. Screenshot-lane only, deliberately:
    /// vizier keeps running through idle time (see `capture_once`'s use of
    /// this field for why), so this never gates `capture_vizier` the way
    /// `capture_blocked` does.
    capture_idle: bool,
    last_capture: Option<CapturedImage>,
    running_flag: Arc<AtomicBool>,
    analysis_tx: Sender<PendingCapture>,
    /// Feeds config + pause changes to the analysis loop's storage sweep.
    config_watch: tokio::sync::watch::Sender<SweepContext>,
    vizier_observer: Box<dyn Observer>,
    vizier_previous: Option<Observation>,
    vizier_session_id: Uuid,
    /// Non-disableable sensitive-path rules, applied to the vizier lane's
    /// fs-events so credential/key paths never leave via activity capture
    /// (the file lane already refuses them; this closes the same hole here).
    sensitive_paths: crate::watcher::ignore::IgnoreRules,
    /// Background task running the file-tracking lane, when active.
    file_tracker: Option<tokio::task::JoinHandle<()>>,
    /// Background task running the BlueBubbles lane, when active.
    bluebubbles: Option<tokio::task::JoinHandle<()>>,
    /// Background task running the local iMessage lane, when active.
    imessage: Option<tokio::task::JoinHandle<()>>,
    /// Background task running the device-activity (Knowledge) lane, when
    /// active.
    knowledge: Option<tokio::task::JoinHandle<()>>,
}

impl WorkerState {
    fn new(
        config: AppConfig,
        running_flag: Arc<AtomicBool>,
        analysis_tx: Sender<PendingCapture>,
        config_watch: tokio::sync::watch::Sender<SweepContext>,
    ) -> Self {
        let vizier_observer = create_observer(ObserverConfig {
            watch_path: config.vizier.watch_path.clone(),
            all_connections: config.vizier.all_connections,
        });

        Self {
            config,
            paused: false,
            capture_blocked: false,
            capture_idle: false,
            last_capture: None,
            running_flag,
            analysis_tx,
            config_watch,
            vizier_observer,
            vizier_previous: None,
            // `empty()` seeds only the non-disableable SENSITIVE_PATTERNS.
            sensitive_paths: crate::watcher::ignore::IgnoreRules::empty()
                .expect("building sensitive-path rules"),
            vizier_session_id: Uuid::new_v4(),
            file_tracker: None,
            bluebubbles: None,
            imessage: None,
            knowledge: None,
        }
    }

    fn is_active(&self) -> bool {
        !self.paused
    }

    /// Abort the file-tracking lane if it's running, and wait for it to
    /// actually stop. Awaiting the aborted task guarantees its `Queue`
    /// connection and `WatcherHandle` are dropped before any replacement
    /// opens the same SQLite file — otherwise an in-flight POST in the old
    /// task could double-send or race acks against the new one.
    async fn stop_file_tracker(&mut self) {
        if let Some(handle) = self.file_tracker.take() {
            handle.abort();
            let _ = handle.await; // returns promptly with a Cancelled error
            log::info!("file tracker stopped");
        }
    }

    /// (Re)start the file-tracking lane to match the current config. No-op
    /// (after stopping any prior task) when the lane isn't runnable —
    /// disabled or no watched folders. A missing Memory token still runs
    /// the watcher so edits queue for reconnect.
    async fn restart_file_tracker(&mut self) {
        self.stop_file_tracker().await;
        if !file_tracker::is_runnable(&self.config) {
            log::debug!(
                "file tracker idle (enabled={}, folders={})",
                self.config.file_tracker.enabled,
                self.config.file_tracker.watched_paths.len(),
            );
            return;
        }
        let queue_path = match crate::config::queue_db_path() {
            Ok(p) => p,
            Err(err) => {
                log::error!("cannot locate file-event queue path: {err:?}");
                return;
            }
        };
        let config = self.config.clone();
        self.file_tracker = Some(tokio::spawn(async move {
            if let Err(err) = file_tracker::run(config, queue_path).await {
                log::error!("file tracker terminated: {err:?}");
            }
        }));
    }

    /// Same lifecycle as [`Self::stop_file_tracker`] — abort and await so a
    /// stale poll can't race an in-flight one after a reconfigure.
    async fn stop_bluebubbles(&mut self) {
        if let Some(handle) = self.bluebubbles.take() {
            handle.abort();
            let _ = handle.await;
            log::info!("bluebubbles lane stopped");
        }
    }

    /// (Re)start the BlueBubbles lane to match the current config. No-op
    /// (after stopping any prior task) when `bluebubbles::is_runnable` says
    /// no — disabled, or no host/password entered yet.
    async fn restart_bluebubbles(&mut self) {
        self.stop_bluebubbles().await;
        if !bluebubbles::is_runnable(&self.config) {
            log::debug!(
                "bluebubbles lane idle (enabled={}, host={:?})",
                self.config.bluebubbles.enabled,
                self.config.bluebubbles.host,
            );
            return;
        }
        let cursor_path = match crate::config::bluebubbles_cursor_path() {
            Ok(p) => p,
            Err(err) => {
                log::error!("cannot locate bluebubbles cursor path: {err:?}");
                return;
            }
        };
        let config = self.config.clone();
        self.bluebubbles = Some(tokio::spawn(async move {
            if let Err(err) = bluebubbles::run(config, cursor_path).await {
                log::error!("bluebubbles lane terminated: {err:?}");
            }
        }));
    }

    /// Same lifecycle as [`Self::stop_bluebubbles`].
    async fn stop_imessage(&mut self) {
        if let Some(handle) = self.imessage.take() {
            handle.abort();
            let _ = handle.await;
            log::info!("imessage lane stopped");
        }
    }

    /// (Re)start the local iMessage lane. No-op when `imessage::is_runnable`
    /// says no (disabled) — a missing Full Disk Access grant is instead
    /// checked and logged once *inside* `imessage::run`, so re-toggling the
    /// setting doesn't need its own permission-recheck path here.
    async fn restart_imessage(&mut self) {
        self.stop_imessage().await;
        if !imessage::is_runnable(&self.config) {
            log::debug!(
                "imessage lane idle (enabled={})",
                self.config.imessage.enabled
            );
            return;
        }
        let cursor_path = match crate::config::imessage_cursor_path() {
            Ok(p) => p,
            Err(err) => {
                log::error!("cannot locate imessage cursor path: {err:?}");
                return;
            }
        };
        let config = self.config.clone();
        self.imessage = Some(tokio::spawn(async move {
            if let Err(err) = imessage::run(config, cursor_path).await {
                log::error!("imessage lane terminated: {err:?}");
            }
        }));
    }

    /// Same lifecycle as [`Self::stop_imessage`].
    async fn stop_knowledge(&mut self) {
        if let Some(handle) = self.knowledge.take() {
            handle.abort();
            let _ = handle.await;
            log::info!("knowledge lane stopped");
        }
    }

    /// (Re)start the device-activity lane. No-op when
    /// `knowledge::is_runnable` says no (disabled) — a missing Full Disk
    /// Access grant is checked and logged once *inside* `knowledge::run`,
    /// exactly as the imessage lane does.
    async fn restart_knowledge(&mut self) {
        self.stop_knowledge().await;
        if !knowledge::is_runnable(&self.config) {
            log::debug!(
                "knowledge lane idle (enabled={})",
                self.config.knowledge.enabled
            );
            return;
        }
        let cursor_path = match crate::config::knowledge_cursor_path() {
            Ok(p) => p,
            Err(err) => {
                log::error!("cannot locate knowledge cursor path: {err:?}");
                return;
            }
        };
        let config = self.config.clone();
        self.knowledge = Some(tokio::spawn(async move {
            if let Err(err) = knowledge::run(config, cursor_path).await {
                log::error!("knowledge lane terminated: {err:?}");
            }
        }));
    }

    async fn handle_command(
        &mut self,
        command: WorkerCommand,
        interval: &mut Interval,
    ) -> Result<bool> {
        match command {
            WorkerCommand::UpdateConfig(config) => {
                let previous_watch_path = self.config.vizier.watch_path.clone();
                let previous_all_connections = self.config.vizier.all_connections;
                let previous_interval_secs = self.config.capture.screenshot_interval_secs;
                // Only churn the file lane when something it depends on
                // actually changed — the token-refresh loop pushes configs
                // autonomously (~hourly), and tearing the watcher down for an
                // unrelated change silently misses file events in the gap.
                let file_lane_changed = self.config.file_tracker != config.file_tracker
                    || self.config.identity != config.identity
                    || self.config.server != config.server;
                // Same reasoning as `file_lane_changed`: `server`/`identity`
                // churn the lane too (it needs both to actually ingest), so
                // it's folded into one flag rather than duplicating the
                // condition and risking the two drifting apart.
                let bluebubbles_changed = self.config.bluebubbles != config.bluebubbles
                    || self.config.identity != config.identity
                    || self.config.server != config.server;
                let imessage_changed = self.config.imessage != config.imessage
                    || self.config.identity != config.identity
                    || self.config.server != config.server;
                let knowledge_changed = self.config.knowledge != config.knowledge
                    || self.config.identity != config.identity
                    || self.config.server != config.server;

                self.config = *config;
                self.publish_sweep_context();
                // A fresh tokio interval fires immediately, so rebuilding it on
                // every config push (the token-refresh loop sends one as often
                // as once a minute) would capture on each push instead of on
                // the configured schedule. Rebuild only when the period moves.
                if previous_interval_secs != self.config.capture.screenshot_interval_secs {
                    *interval = build_interval(self.config.capture.screenshot_interval_secs);
                }

                if previous_watch_path != self.config.vizier.watch_path
                    || previous_all_connections != self.config.vizier.all_connections
                {
                    self.reset_vizier_observer();
                }

                // Pick up new watched folders / identity / server token —
                // but never (re)start capture behind a paused user's back.
                if file_lane_changed && self.is_active() {
                    self.restart_file_tracker().await;
                }
                if bluebubbles_changed && self.is_active() {
                    self.restart_bluebubbles().await;
                }
                if imessage_changed && self.is_active() {
                    self.restart_imessage().await;
                }
                if knowledge_changed && self.is_active() {
                    self.restart_knowledge().await;
                }
            }
            WorkerCommand::Pause => {
                self.paused = true;
                self.running_flag.store(false, Ordering::SeqCst);
                self.publish_sweep_context();
                self.stop_file_tracker().await;
                self.stop_bluebubbles().await;
                self.stop_imessage().await;
                self.stop_knowledge().await;
            }
            WorkerCommand::Resume => {
                self.paused = false;
                self.running_flag.store(true, Ordering::SeqCst);
                self.publish_sweep_context();
                self.restart_file_tracker().await;
                self.restart_bluebubbles().await;
                self.restart_imessage().await;
                self.restart_knowledge().await;
            }
            WorkerCommand::CaptureNow => {
                let capture_tick_id = Uuid::new_v4();
                if let Err(err) = self.capture_once(capture_tick_id).await {
                    log::error!("manual capture failed: {err:?}");
                }
                if self.config.vizier.enabled
                    && !self.capture_blocked
                    && let Err(err) = self.capture_vizier(capture_tick_id).await
                {
                    log::error!("manual vizier capture failed: {err:?}");
                }
            }
            WorkerCommand::Shutdown => {
                self.stop_file_tracker().await;
                self.stop_bluebubbles().await;
                self.stop_imessage().await;
                self.stop_knowledge().await;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Push the current config + pause state to the analysis loop's sweep.
    fn publish_sweep_context(&self) {
        let _ = self.config_watch.send(SweepContext {
            config: self.config.clone(),
            active: !self.paused,
        });
    }

    fn reset_vizier_observer(&mut self) {
        self.vizier_observer = create_observer(ObserverConfig {
            watch_path: self.config.vizier.watch_path.clone(),
            all_connections: self.config.vizier.all_connections,
        });
        self.vizier_previous = None;
        self.vizier_session_id = Uuid::new_v4();
    }

    async fn capture_vizier(&mut self, capture_tick_id: Uuid) -> Result<()> {
        let server_configured = server_ingest::is_configured(&self.config);
        let (user_id, device_id) = match (
            self.config.identity.user_id,
            self.config.identity.device_id,
        ) {
            (Some(user), Some(device)) => (user, device),
            // OIDC sign-in completed but the deferred device registration
            // hasn't landed yet — skip rather than emit nil-attributed events
            // every tick during that window; the refresh loop registers the
            // device and the next tick sends properly.
            _ if self.config.oidc_session.is_some() && self.config.identity.device_id.is_none() => {
                log::debug!("vizier: awaiting device registration; skipping tick");
                return Ok(());
            }
            // API-key installs may legitimately have no local device id (the
            // server attributes by credential) — fall back rather than starve
            // the activity lane.
            _ if server_configured => (
                self.config.identity.user_id.unwrap_or_else(Uuid::nil),
                self.config.identity.device_id.unwrap_or_else(Uuid::nil),
            ),
            _ => {
                log::warn!(
                    "identity not configured; skipping vizier capture (set user and device IDs in settings)"
                );
                return Ok(());
            }
        };

        let mut current = self
            .vizier_observer
            .snapshot()
            .context("capturing vizier snapshot")?;

        // Drop fs-events for sensitive paths before they touch the diff, the
        // payload, or the stored baseline — the activity lane must honour the
        // same credential/key exclusions as the file lane.
        let before = current.fs_events.len();
        current
            .fs_events
            .retain(|event| !self.sensitive_paths.is_sensitive(&event.path));
        let dropped = before - current.fs_events.len();
        if dropped > 0 {
            log::debug!("vizier: dropped {dropped} sensitive fs-event path(s) before send");
        }

        // Nothing observed this tick: no focus, no windows, no file or network
        // deltas. This is the steady state on Windows and non-Hyprland Linux,
        // where vizier has no desktop observer and the baseline honestly
        // reports no windows. Shipping it would only tell Memory "nothing"
        // every tick, so skip the POST — but only once the server already
        // holds an empty state (`vizier_should_skip_send`): the first empty
        // tick after a non-empty one is sent so the previous fs events /
        // windows are cleared rather than left "current" indefinitely. The
        // baseline (`vizier_previous`) is deliberately left alone on a skip:
        // the server still holds the last state we actually sent, so the next
        // non-empty tick diffs against that.
        if vizier_should_skip_send(self.vizier_previous.as_ref(), &current) {
            let skipped = VIZIER_EMPTY_SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if !VIZIER_EMPTY_WARNED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "vizier: empty observation (no focus/windows/fs/net); skipping send — \
                     expected on Windows and non-Hyprland Linux, where there is no desktop \
                     observer and the activity lane ships only file/network deltas"
                );
            } else {
                log::debug!(
                    "vizier: empty observation (no focus/windows/fs/net); skipping send \
                     ({skipped} skipped since launch)"
                );
            }
            return Ok(());
        }

        let focus_app = current.focus.as_ref().map(|window| window.app.clone());
        let focus_title = current.focus.as_ref().map(|window| window.title.clone());
        let cwd = current.terminal_ctx.as_ref().map(|ctx| ctx.cwd.clone());
        let fs_event_count = current.fs_events.len() as i32;
        let observed_at = Utc::now();

        let (event_kind, monotonic_ms, payload) = match &self.vizier_previous {
            None => (
                "snapshot".to_string(),
                current.monotonic_ms as i64,
                serde_json::to_value(&current).context("serializing vizier snapshot")?,
            ),
            Some(previous) => {
                let diff = create_diff_envelope(previous, &current)
                    .context("building vizier diff envelope")?;
                (
                    "patch".to_string(),
                    diff.monotonic_ms as i64,
                    serde_json::to_value(&diff).context("serializing vizier diff")?,
                )
            }
        };

        let event = VizierEventRecord {
            user_id,
            device_id,
            session_id: self.vizier_session_id,
            event_kind,
            observed_at,
            monotonic_ms,
            payload,
            focus_app,
            focus_title,
            cwd,
            fs_event_count,
            capture_tick_id: Some(capture_tick_id),
        };

        if server_configured {
            // Guard against payloads CI-Server's JSON body limit will reject.
            let payload_bytes = serde_json::to_vec(&event.payload)
                .map(|v| v.len())
                .unwrap_or(0);
            if payload_bytes > MAX_VIZIER_PAYLOAD_BYTES {
                log::warn!(
                    "vizier {} payload is {:.1} MB (over the {} MB cap); skipping send",
                    event.event_kind,
                    payload_bytes as f64 / 1_048_576.0,
                    MAX_VIZIER_PAYLOAD_BYTES / 1_048_576
                );
                // Oversized: the server will never receive this state, so the
                // baseline must not advance to it. Drop back to None so the next
                // tick sends a full snapshot, and start a new session so the
                // server sees an explicit discontinuity rather than silently
                // applying a patch to a state it never held.
                self.vizier_previous = None;
                self.vizier_session_id = Uuid::new_v4();
                return Ok(());
            } else if let Err(err) =
                server_ingest::ingest_vizier_event(&self.config, device_id, &event).await
            {
                // Keep the event on disk and reset the baseline so the next
                // live tick is a snapshot, not a patch against state Memory
                // never held. The store sweep sends the parked event on
                // reconnect.
                if let Err(persist_err) = persist_vizier_event(&self.config, &event) {
                    log::warn!("failed to park vizier event: {persist_err:?}");
                }
                log::warn!("failed to write vizier event via CI-Server, parked for retry: {err:?}");
                self.vizier_previous = None;
                self.vizier_session_id = Uuid::new_v4();
                return Ok(());
            }
        } else {
            #[cfg(feature = "legacy-postgres")]
            if let Some(recorder) = build_legacy_recorder(&self.config) {
                if let Err(err) = recorder.record_vizier_event(event.clone()).await {
                    log::warn!("failed to write vizier event to postgres: {err:?}");
                }
            }
            // No Memory yet: park the event the same way screenshots stay on
            // disk. The sweep drains it after sign-in.
            if let Err(err) = persist_vizier_event(&self.config, &event) {
                log::warn!("failed to park vizier event while disconnected: {err:?}");
            } else {
                log::debug!(
                    "no server credential; parked vizier {} for reconnect",
                    event.event_kind
                );
            }
        }

        // Advance the baseline only on a successful send (every failure path
        // above returns early after resetting it). The server and the applet now
        // agree on what state the next diff is relative to.
        self.vizier_previous = Some(current);
        Ok(())
    }

    async fn capture_once(&mut self, capture_tick_id: Uuid) -> Result<bool> {
        if is_capture_blocked() {
            if !self.capture_blocked {
                log::info!("screensaver active; skipping capture ticks until desktop is unlocked");
                self.capture_blocked = true;
            }
            return Ok(false);
        }

        if self.capture_blocked {
            log::info!("desktop unlocked; resuming capture ticks");
            self.capture_blocked = false;
        }

        // Idle gate: screenshot-lane only. An unlocked-but-untouched desktop
        // (no keyboard/mouse input for a full capture interval) has nothing
        // new to screenshot, but it is not the same state as a locked one —
        // vizier's focus/window/fs activity can still be worth timelining
        // during a long idle stretch (a build running, a video playing), so
        // this intentionally does not set `capture_blocked` and does not
        // touch `capture_vizier`'s gating. Folding vizier into an idle gate
        // too is a larger, separate decision tracked as CI-Engineering
        // #283/#293, not this change.
        //
        // `idle_ms() == None` (no probe on this OS, e.g. Linux, or a probe
        // failure on Windows/macOS) must never block capture — absence of a
        // signal is not evidence of idleness — see `is_idle_past_interval`.
        if is_idle_past_interval(
            crate::lock_detect::idle_ms(),
            self.config.capture.screenshot_interval_secs,
        ) {
            if !self.capture_idle {
                log::info!("system idle; skipping capture ticks");
                self.capture_idle = true;
            }
            return Ok(false);
        }

        if self.capture_idle {
            log::info!("activity resumed; resuming capture ticks");
            self.capture_idle = false;
        }

        if !server_ingest::is_configured(&self.config)
            && (self.config.identity.user_id.is_none() || self.config.identity.device_id.is_none())
        {
            log::warn!(
                "identity not configured; skipping capture (set user and device IDs in settings)"
            );
            return Ok(false);
        }

        let capture = tokio::task::spawn_blocking(capture_primary)
            .await
            .context("joining capture thread")?
            .context("capturing display")?;

        let diff_percent = self
            .last_capture
            .as_ref()
            .map(|prev| diff_percent(prev, &capture))
            .unwrap_or(100.0);

        self.last_capture = Some(capture.clone());

        if diff_percent < self.config.capture.diff_threshold_percent {
            log::debug!(
                "diff {:.2}% below threshold {:.2}%",
                diff_percent,
                self.config.capture.diff_threshold_percent
            );
            return Ok(false);
        }

        let timestamp = Utc::now();
        let slug = timestamp.format("%Y%m%dT%H%M%S");
        // The trailing UUID is `capture_tick_id`, not a fresh random one — the
        // disk-backed retry sweep (capture_store::scan) only ever recovers a
        // filename + bytes, so this is how the correlator survives a deferred
        // upload or a process restart without a new sidecar file. See
        // server_ingest::capture_upload_form, which parses it back out.
        let filename = format!("capture_{}_{}.png", slug, capture_tick_id);
        let local_path = self.config.capture.storage_dir.join(&filename);

        let capture_clone = capture.clone();
        tokio::task::spawn_blocking(move || save_to_path(&capture_clone, &local_path))
            .await
            .context("joining save thread")?
            .context("saving screenshot")?;

        let pending = PendingCapture {
            image: capture,
            filename,
            timestamp,
            config: self.config.clone(),
        };

        if let Err(e) = self.analysis_tx.try_send(pending) {
            // Not lost: the PNG is already saved in storage_dir, and the
            // analysis loop's periodic sweep re-uploads unsent frames.
            log::warn!("analysis queue is full or closed; deferring capture to store sweep: {e}");
        }

        Ok(true)
    }
}

#[cfg(feature = "legacy-postgres")]
fn build_legacy_recorder(config: &AppConfig) -> Option<PostgresRecorder> {
    match PostgresRecorder::new(config) {
        Ok(recorder) => Some(recorder),
        Err(err) => {
            log::error!("failed to initialize postgres pool: {err:?}");
            None
        }
    }
}

#[cfg(feature = "legacy-minio")]
async fn record_capture_legacy(
    capture: &PendingCapture,
    png_bytes: &[u8],
    analysis: &AnalysisResult,
) -> Result<()> {
    let (user_id, device_id) = match (
        capture.config.identity.user_id,
        capture.config.identity.device_id,
    ) {
        (Some(user), Some(device)) => (user, device),
        _ => {
            log::warn!("identity not configured for legacy capture ingest; dropping capture");
            return Ok(());
        }
    };

    let uploader = MinioUploader::new(&capture.config.minio)
        .await
        .context("creating legacy minio uploader")?;
    let recorder = match build_legacy_recorder(&capture.config) {
        Some(recorder) => recorder,
        None => return Ok(()),
    };

    let file_size = png_bytes.len() as i64;
    let date_path = capture.timestamp.format("%Y/%m/%d");
    let key = format!(
        "captures/{}/{}/{}/{}",
        user_id, device_id, date_path, capture.filename
    );

    let remote_url = uploader
        .upload_png(&key, png_bytes.to_vec())
        .await
        .context("uploading screenshot to legacy minio")?;

    let record = ScreenshotRecord {
        captured_at: capture.timestamp,
        user_id,
        device_id,
        remote_path: remote_url,
        filename: capture.filename.clone(),
        file_size_bytes: file_size,
    };

    recorder
        .record_capture(record, analysis)
        .await
        .context("writing legacy postgres rows")?;

    Ok(())
}

fn is_capture_blocked() -> bool {
    crate::lock_detect::is_locked()
}

/// The screenshot lane's idle-skip decision, factored out of `capture_once`
/// so it is testable without a live `WorkerState` or a real capture
/// pipeline. `idle_ms` is `lock_detect::idle_ms()`'s result verbatim: `None`
/// (no probe on this OS, or a probe failure) must never read as idle —
/// absence of a signal is not evidence of idleness — so only a concrete
/// `Some` at or past a full capture interval counts.
fn is_idle_past_interval(idle_ms: Option<u64>, screenshot_interval_secs: u64) -> bool {
    match idle_ms {
        Some(ms) => ms >= screenshot_interval_secs.saturating_mul(1000),
        None => false,
    }
}

#[cfg(test)]
mod idle_gate_tests {
    use super::is_idle_past_interval;

    /// The one case that matters most: no idle signal at all — Linux always,
    /// or a Windows/macOS probe failure — must never be treated as idle.
    /// This is the guarantee `capture_once` depends on to never wedge the
    /// screenshot lane on a platform that simply cannot answer the question.
    #[test]
    fn no_idle_signal_never_counts_as_idle() {
        assert!(!is_idle_past_interval(None, 300));
        // Even a zero-length interval (misconfigured, but not this
        // function's job to reject) must not turn "no signal" into "idle".
        assert!(!is_idle_past_interval(None, 0));
    }

    #[test]
    fn idle_below_the_capture_interval_does_not_gate() {
        assert!(!is_idle_past_interval(Some(299_999), 300));
    }

    /// The gate is inclusive: exactly one interval's worth of idle time is
    /// enough, matching the `>=` in `is_idle_past_interval`.
    #[test]
    fn idle_at_or_past_the_capture_interval_gates() {
        assert!(is_idle_past_interval(Some(300_000), 300));
        assert!(is_idle_past_interval(Some(600_000), 300));
    }

    /// `screenshot_interval_secs * 1000` must not silently wrap on a
    /// pathological config value; `saturating_mul` pins the threshold at
    /// `u64::MAX` instead of wrapping around to a small number, which would
    /// have made an absurd interval gate on almost no idle time at all.
    #[test]
    fn absurd_interval_saturates_instead_of_wrapping() {
        // Wrapping multiplication would send the threshold near 0 (undergoing
        // several `u64` wraps), so a merely large — not maximal — idle
        // reading would wrongly clear it; saturation instead pins the
        // threshold at `u64::MAX`, so only an equally-maximal idle reading
        // can meet it.
        assert!(!is_idle_past_interval(Some(u64::MAX - 1), u64::MAX));
        assert!(is_idle_past_interval(Some(u64::MAX), u64::MAX));
    }
}

#[cfg(test)]
mod sensitive_scrub_tests {
    use vizier::observation::FSEvent;

    use crate::watcher::ignore::IgnoreRules;

    fn fs(path: &str) -> FSEvent {
        FSEvent {
            path: path.to_string(),
            kind: "modify".to_string(),
            ts: 0.0,
            file_activity: None,
        }
    }

    /// The exact predicate capture_vizier applies to the vizier lane's
    /// fs-events: credential/key paths the file lane refuses must also be
    /// dropped here.
    #[test]
    fn sensitive_fs_event_paths_are_dropped() {
        let rules = IgnoreRules::empty().unwrap();
        let mut events = vec![
            fs("/Users/x/.ssh/id_rsa"),
            fs("/Users/x/project/.env"),
            fs("/Users/x/certs/server.pem"),
            fs("/Users/x/.aws/credentials"),
            fs("/Users/x/Documents/notes.md"), // benign — must survive
            fs("/Users/x/src/main.rs"),        // benign — must survive
        ];
        events.retain(|e| !rules.is_sensitive(&e.path));
        let kept: Vec<&str> = events.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            kept,
            vec!["/Users/x/Documents/notes.md", "/Users/x/src/main.rs"]
        );
    }
}

#[cfg(test)]
mod vizier_empty_observation_tests {
    use vizier::observation::{
        Bounds, ConnInfo, DisplayInfo, FSEvent, Observation, Point, TerminalCtx, WindowInfo,
    };

    use super::{vizier_empty_ticks_skipped, vizier_observation_is_empty, vizier_should_skip_send};

    /// An observation with only the fields the baseline observer always
    /// fills — timestamps, a zero cursor, no focus/windows/fs/net.
    fn bare() -> Observation {
        Observation {
            schema_version: 1,
            ts: 1.0,
            monotonic_ms: 10,
            idle_ms: 0,
            focus: None,
            windows: Vec::new(),
            cursor: Point { x: 0, y: 0 },
            displays: Vec::new(),
            terminal_ctx: None,
            net_connections: Vec::new(),
            fs_events: Vec::new(),
        }
    }

    fn window() -> WindowInfo {
        WindowInfo {
            id: "0x1".to_string(),
            title: "notes.md".to_string(),
            app: "Editor".to_string(),
            pid: 42,
            bounds: Bounds {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
            },
            workspace: 1,
            is_minimized: false,
            is_fullscreen: false,
        }
    }

    /// The Windows / non-Hyprland-Linux steady state: nothing observed, so the
    /// tick must be skipped rather than POSTed as an all-empty snapshot.
    #[test]
    fn observation_with_no_focus_windows_fs_or_net_is_empty() {
        assert!(vizier_observation_is_empty(&bare()));
    }

    /// Placeholder fields alone (zero-sized display, the observing process's
    /// own cwd/shell) must not rescue an otherwise empty observation — they
    /// are identical every tick and carry no activity signal.
    #[test]
    fn placeholder_display_and_terminal_ctx_do_not_make_it_non_empty() {
        let mut observation = bare();
        observation.displays = vec![DisplayInfo {
            id: 0,
            bounds: Bounds {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            is_primary: true,
            scale_factor: 1.0,
        }];
        observation.terminal_ctx = Some(TerminalCtx {
            cwd: "/".to_string(),
            shell: "/bin/zsh".to_string(),
        });
        assert!(vizier_observation_is_empty(&observation));
    }

    /// Each real signal on its own is enough to ship the tick: a focused
    /// window, a window list, a filesystem delta, or a network connection.
    #[test]
    fn any_real_signal_makes_the_observation_non_empty() {
        let mut with_focus = bare();
        with_focus.focus = Some(window());
        assert!(!vizier_observation_is_empty(&with_focus));

        let mut with_windows = bare();
        with_windows.windows = vec![window()];
        assert!(!vizier_observation_is_empty(&with_windows));

        let mut with_fs = bare();
        with_fs.fs_events = vec![FSEvent {
            path: "/home/x/notes.md".to_string(),
            kind: "Modify".to_string(),
            ts: 1.0,
            file_activity: None,
        }];
        assert!(!vizier_observation_is_empty(&with_fs));

        let mut with_net = bare();
        with_net.net_connections = vec![ConnInfo {
            proto: "tcp".to_string(),
            local_port: 50000,
            remote_addr: "203.0.113.1".to_string(),
            remote_port: 443,
            pid: 42,
            app: "browser".to_string(),
            state: "ESTABLISHED".to_string(),
            connection_count: None,
            remote_host_count: None,
        }];
        assert!(!vizier_observation_is_empty(&with_net));
    }

    /// The skip must not swallow the transition *into* emptiness: after a
    /// tick that shipped fs events (or windows), the next empty tick is sent
    /// once so the server clears them, and only the tick after that is
    /// skipped. Without a baseline (first tick, or after a failed POST reset
    /// it) an empty tick is skipped straight away — the Windows steady state.
    #[test]
    fn empty_tick_after_a_non_empty_baseline_is_sent_once_then_skipped() {
        let empty = bare();

        // No baseline yet: nothing to clear, skip.
        assert!(vizier_should_skip_send(None, &empty));

        // Baseline carried a file delta: ship this empty tick to clear it.
        let mut with_fs = bare();
        with_fs.fs_events = vec![FSEvent {
            path: "/home/x/notes.md".to_string(),
            kind: "Modify".to_string(),
            ts: 1.0,
            file_activity: None,
        }];
        assert!(!vizier_should_skip_send(Some(&with_fs), &empty));

        // Baseline carried a focused window (macOS, everything just closed):
        // ship the all-closed transition.
        let mut with_focus = bare();
        with_focus.focus = Some(window());
        assert!(!vizier_should_skip_send(Some(&with_focus), &empty));

        // Once the server holds an empty state, later empty ticks are skipped.
        assert!(vizier_should_skip_send(Some(&empty), &empty));

        // A non-empty current tick is never skipped, whatever the baseline.
        assert!(!vizier_should_skip_send(None, &with_fs));
        assert!(!vizier_should_skip_send(Some(&empty), &with_fs));
        assert!(!vizier_should_skip_send(Some(&with_fs), &with_focus));
    }

    /// The counter the health row reads starts at zero: nothing in the test
    /// binary drives `capture_vizier`, so no tick has been skipped.
    #[test]
    fn skipped_tick_counter_starts_at_zero() {
        assert_eq!(vizier_empty_ticks_skipped(), 0);
    }
}
