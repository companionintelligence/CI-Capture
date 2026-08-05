use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::capture::{CapturedImage, capture_primary, diff_percent, save_to_path};
use crate::config::AppConfig;
#[cfg(feature = "legacy-postgres")]
use crate::db::PostgresRecorder;
#[cfg(feature = "legacy-minio")]
use crate::db::ScreenshotRecord;
use crate::db::VizierEventRecord;
use crate::file_tracker;
#[cfg(feature = "legacy-minio")]
use crate::minio::MinioUploader;
use crate::server_ingest;

const MIN_INTERVAL_SECS: u64 = 30;
const ANALYSIS_QUEUE_CAPACITY: usize = 2;
/// Largest vizier payload we'll attempt to POST. Sits under CI-Server's 10 MB
/// ingest body limit so an oversized snapshot is skipped, not bounced as 413.
const MAX_VIZIER_PAYLOAD_BYTES: usize = 9 * 1024 * 1024;

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
                if let Err(err) = state.capture_once().await {
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
                    && let Err(err) = state.capture_vizier().await
                {
                    log::error!("vizier worker error: {err:?}");
                }
            }
        }
    }

    Ok(())
}

/// How often the analysis loop sweeps `storage_dir` for retention pruning
/// and backlog re-upload.
const STORE_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

async fn run_analysis_loop(
    rx: &mut Receiver<PendingCapture>,
    config_watch: tokio::sync::watch::Receiver<SweepContext>,
) {
    let analyzer = VisionAnalyzer::default();
    let mut sweep_tick = time::interval(STORE_SWEEP_INTERVAL);
    sweep_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(capture) = maybe else { break };
                handle_fresh_capture(&analyzer, &capture).await;
            }
            _ = sweep_tick.tick() => {
                let ctx = config_watch.borrow().clone();
                sweep_capture_store(&analyzer, &ctx).await;
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
async fn sweep_capture_store(analyzer: &VisionAnalyzer, ctx: &SweepContext) {
    let config = &ctx.config;
    let storage_dir = config.capture.storage_dir.clone();
    let entries = tokio::task::spawn_blocking(move || crate::capture_store::scan(&storage_dir))
        .await
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }

    let plan =
        crate::capture_store::plan_sweep(entries, &config.capture, std::time::SystemTime::now());

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
    /// identity unset, no watched folders, or no server token.
    async fn restart_file_tracker(&mut self) {
        self.stop_file_tracker().await;
        if !file_tracker::is_runnable(&self.config) {
            log::debug!(
                "file tracker idle (enabled={}, folders={}, server={})",
                self.config.file_tracker.enabled,
                self.config.file_tracker.watched_paths.len(),
                server_ingest::is_configured(&self.config),
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

    async fn handle_command(
        &mut self,
        command: WorkerCommand,
        interval: &mut Interval,
    ) -> Result<bool> {
        match command {
            WorkerCommand::UpdateConfig(config) => {
                let previous_watch_path = self.config.vizier.watch_path.clone();
                let previous_all_connections = self.config.vizier.all_connections;
                // Only churn the file lane when something it depends on
                // actually changed — the token-refresh loop pushes configs
                // autonomously (~hourly), and tearing the watcher down for an
                // unrelated change silently misses file events in the gap.
                let file_lane_changed = self.config.file_tracker != config.file_tracker
                    || self.config.identity != config.identity
                    || self.config.server != config.server;

                self.config = *config;
                self.publish_sweep_context();
                *interval = build_interval(self.config.capture.screenshot_interval_secs);

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
            }
            WorkerCommand::Pause => {
                self.paused = true;
                self.running_flag.store(false, Ordering::SeqCst);
                self.publish_sweep_context();
                self.stop_file_tracker().await;
            }
            WorkerCommand::Resume => {
                self.paused = false;
                self.running_flag.store(true, Ordering::SeqCst);
                self.publish_sweep_context();
                self.restart_file_tracker().await;
            }
            WorkerCommand::CaptureNow => {
                if let Err(err) = self.capture_once().await {
                    log::error!("manual capture failed: {err:?}");
                }
                if self.config.vizier.enabled
                    && !self.capture_blocked
                    && let Err(err) = self.capture_vizier().await
                {
                    log::error!("manual vizier capture failed: {err:?}");
                }
            }
            WorkerCommand::Shutdown => {
                self.stop_file_tracker().await;
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

    async fn capture_vizier(&mut self) -> Result<()> {
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
                // Same reasoning as the oversize path. This lane is
                // fire-and-forget with no durable queue, so an unacknowledged
                // send is simply lost — and advancing the baseline anyway would
                // make every subsequent tick a patch against a state the server
                // never received, desyncing its view of the activity timeline
                // permanently. On a laptop (sleep/wake, changing networks) that
                // is routine, not rare. Resetting costs one larger snapshot on
                // the next tick and self-heals on the first reconnect.
                log::warn!(
                    "failed to write vizier event via CI-Server, resetting baseline: {err:?}"
                );
                self.vizier_previous = None;
                self.vizier_session_id = Uuid::new_v4();
                return Ok(());
            }
        } else {
            #[cfg(feature = "legacy-postgres")]
            if let Some(recorder) = build_legacy_recorder(&self.config) {
                if let Err(err) = recorder.record_vizier_event(event).await {
                    log::warn!("failed to write vizier event to postgres: {err:?}");
                }
            }
            // Default build: there is deliberately no fallback. Writing activity
            // data — window titles, terminal cwd, filesystem deltas — from the
            // client straight into a plaintext database is precisely what the
            // server-relay model exists to prevent. Say so once per tick at
            // debug, and let the tray's "⚠︎ Setup needed" status carry the
            // user-visible signal.
            #[cfg(not(feature = "legacy-postgres"))]
            log::debug!(
                "no server credential; dropping vizier {} (activity capture needs a Companion Memory server)",
                event.event_kind
            );
        }

        // Advance the baseline only on a successful send (every failure path
        // above returns early after resetting it). The server and the applet now
        // agree on what state the next diff is relative to.
        self.vizier_previous = Some(current);
        Ok(())
    }

    async fn capture_once(&mut self) -> Result<bool> {
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
        let filename = format!("capture_{}_{}.png", slug, Uuid::new_v4());
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
    #[cfg(target_os = "macos")]
    {
        return is_macos_screensaver_running();
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn is_macos_screensaver_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "ScreenSaverEngine"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
