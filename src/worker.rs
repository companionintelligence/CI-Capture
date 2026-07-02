use std::process::Command;
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
#[cfg(feature = "legacy-minio")]
use crate::db::ScreenshotRecord;
use crate::db::{PostgresRecorder, VizierEventRecord};
use crate::file_tracker;
#[cfg(feature = "legacy-minio")]
use crate::minio::MinioUploader;
use crate::server_ingest;

const MIN_INTERVAL_SECS: u64 = 30;
const ANALYSIS_QUEUE_CAPACITY: usize = 2;
/// Largest vizier payload we'll attempt to POST. Sits under CI-Server's 10 MB
/// ingest body limit so an oversized snapshot is skipped, not bounced as 413.
const MAX_VIZIER_PAYLOAD_BYTES: usize = 9 * 1024 * 1024;

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
        let _ = self.tx.send(WorkerCommand::UpdateConfig(config));
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
    UpdateConfig(AppConfig),
    Pause,
    Resume,
    CaptureNow,
    Shutdown,
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

    tokio::spawn(async move {
        run_analysis_loop(&mut analysis_rx).await;
    });

    let mut state = WorkerState::new(initial_config, running_flag, analysis_tx);
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
                match state.capture_once().await {
                    Ok(captured) => {
                        if captured && state.config.vizier.enabled {
                            if let Err(err) = state.capture_vizier().await {
                                log::error!("vizier worker error: {err:?}");
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("screenshot worker error: {err:?}");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_analysis_loop(rx: &mut Receiver<PendingCapture>) {
    let analyzer = VisionAnalyzer::default();

    while let Some(capture) = rx.recv().await {
        let png_bytes = match capture.image.to_png_bytes() {
            Ok(b) => b,
            Err(e) => {
                log::error!("failed to encode png for analysis: {e:?}");
                continue;
            }
        };

        if server_ingest::is_configured(&capture.config) {
            ingest_capture_to_server(&analyzer, &capture, &png_bytes).await;
            continue;
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
        if let Err(err) = record_capture_legacy(&capture, &png_bytes, &analysis).await {
            log::error!("failed to write legacy capture rows: {err:?}");
        }
        #[cfg(not(feature = "legacy-minio"))]
        {
            let _ = &analysis;
            log::warn!(
                "no CI-Server token configured; screenshot dropped (rebuild with --features legacy-minio for the MinIO fallback)"
            );
        }
    }
}

/// Upload a capture to CI-Server. When the applet has its own vision endpoint
/// configured, it analyses the screenshot locally and writes the result onto
/// the file as title/description (CI-Server skips its own Ollama analysis).
/// Otherwise it asks CI-Server to analyse the image itself (Ollama fallback).
async fn ingest_capture_to_server(
    analyzer: &VisionAnalyzer,
    capture: &PendingCapture,
    png_bytes: &[u8],
) {
    let config = &capture.config;
    let has_endpoint = !config.analysis.endpoint.trim().is_empty();

    // `inventoryCapture` doubles as "let the server analyse this": false when we
    // analysed locally, true to invoke the server's Ollama fallback. When we
    // analysed locally we also set `skipDescription` so CI-Server doesn't
    // redundantly re-describe the image — we PATCH our own result below.
    let file_id = match server_ingest::ingest_capture(
        config,
        &capture.filename,
        png_bytes,
        !has_endpoint,
        has_endpoint,
        None,
    )
    .await
    {
        Ok(id) => id,
        Err(err) => {
            log::error!(
                "failed to ingest screenshot: {err:?} (file={}, size={:.1} KB)",
                capture.filename,
                png_bytes.len() as f64 / 1024.0
            );
            return;
        }
    };

    if !has_endpoint {
        return; // Server runs its own analysis.
    }

    let analysis = match analyzer.analyze(&config.analysis, png_bytes).await {
        Ok(result) => result,
        Err(err) => {
            // The screenshot is already stored; just skip metadata this round.
            log::warn!("vision analysis failed: {err:?}");
            return;
        }
    };

    let title = analysis
        .primary_application
        .clone()
        .or_else(|| analysis.activity_category.clone())
        .unwrap_or_else(|| "Screen capture".to_string());
    let description = analysis.activity_summary.clone();

    if let Err(err) = server_ingest::set_file_metadata(
        config,
        &file_id,
        Some(&title),
        description.as_deref(),
    )
    .await
    {
        log::warn!("failed to set file metadata for {file_id}: {err:?}");
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
    vizier_observer: Box<dyn Observer>,
    vizier_previous: Option<Observation>,
    vizier_session_id: Uuid,
    /// Background task running the file-tracking lane, when active.
    file_tracker: Option<tokio::task::JoinHandle<()>>,
}

impl WorkerState {
    fn new(
        config: AppConfig,
        running_flag: Arc<AtomicBool>,
        analysis_tx: Sender<PendingCapture>,
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
            vizier_observer,
            vizier_previous: None,
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

                self.config = config;
                *interval = build_interval(self.config.capture.screenshot_interval_secs);

                if previous_watch_path != self.config.vizier.watch_path
                    || previous_all_connections != self.config.vizier.all_connections
                {
                    self.reset_vizier_observer();
                }

                // Pick up new watched folders / identity / server token.
                self.restart_file_tracker().await;
            }
            WorkerCommand::Pause => {
                self.paused = true;
                self.running_flag.store(false, Ordering::SeqCst);
                self.stop_file_tracker().await;
            }
            WorkerCommand::Resume => {
                self.paused = false;
                self.running_flag.store(true, Ordering::SeqCst);
                self.restart_file_tracker().await;
            }
            WorkerCommand::CaptureNow => match self.capture_once().await {
                Ok(captured) => {
                    if captured && self.config.vizier.enabled {
                        if let Err(err) = self.capture_vizier().await {
                            log::error!("manual vizier capture failed: {err:?}");
                        }
                    }
                }
                Err(err) => {
                    log::error!("manual capture failed: {err:?}");
                }
            },
            WorkerCommand::Shutdown => {
                self.stop_file_tracker().await;
                return Ok(true);
            }
        }
        Ok(false)
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

        let current = self
            .vizier_observer
            .snapshot()
            .context("capturing vizier snapshot")?;

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
            // Skipping (or failing) is non-fatal and we still advance the
            // baseline below, so we never wedge re-sending the same oversized
            // snapshot every tick.
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
            } else if let Err(err) =
                server_ingest::ingest_vizier_event(&self.config, device_id, &event).await
            {
                log::warn!("failed to write vizier event via CI-Server: {err:?}");
            }
        } else if let Some(recorder) = build_legacy_recorder(&self.config) {
            if let Err(err) = recorder.record_vizier_event(event).await {
                log::warn!("failed to write vizier event to postgres: {err:?}");
            }
        }

        // Always advance the baseline — even on skip/failure — so the next tick
        // produces a small diff rather than re-sending this snapshot.
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
            log::warn!("analysis queue is full or closed; dropping capture: {e}");
        }

        Ok(true)
    }
}

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
