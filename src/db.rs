use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use deadpool_postgres::{Client, Config, ManagerConfig, Pool, RecyclingMethod};
use serde_json::{Value, json};
use tokio_postgres::{NoTls, types::{Json, ToSql}};
use uuid::Uuid;

use crate::analysis::AnalysisResult;
use crate::config::AppConfig;

#[derive(Clone)]
pub struct ScreenshotRecord {
    pub captured_at: DateTime<Utc>,
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub remote_path: String,
    pub filename: String,
    pub file_size_bytes: i64,
}

#[derive(Clone)]
pub struct VizierEventRecord {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub session_id: Uuid,
    pub event_kind: String,
    pub observed_at: DateTime<Utc>,
    pub monotonic_ms: i64,
    pub payload: Value,
    pub focus_app: Option<String>,
    pub focus_title: Option<String>,
    pub cwd: Option<String>,
    pub fs_event_count: i32,
}

#[derive(Clone)]
pub struct PostgresRecorder {
    pool: Pool<NoTls>,
    config: AppConfig,
    vizier_schema_ready: Arc<AtomicBool>,
}

impl PostgresRecorder {
    pub fn new(config: &AppConfig) -> Result<Self> {
        let mut pg_config = Config::new();
        pg_config.host = Some(config.postgres.host.clone());
        pg_config.port = Some(config.postgres.port);
        pg_config.user = Some(config.postgres.user.clone());
        pg_config.password = Some(config.postgres.password.clone());
        pg_config.dbname = Some(config.postgres.database.clone());
        pg_config.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });

        let pool = pg_config
            .create_pool(NoTls)
            .context("creating postgres pool")?;

        Ok(Self {
            pool,
            config: config.clone(),
            vizier_schema_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn record_capture(
        &self,
        metadata: ScreenshotRecord,
        analysis: &AnalysisResult,
    ) -> Result<()> {
        let client = self.pool.get().await.context("getting postgres connection")?;

        let file_id = self
            .insert_file_record(&client, &metadata)
            .await
            .context("inserting file metadata")?;

        self.insert_screen_capture(&client, &metadata, analysis, file_id)
            .await
            .context("inserting screen capture")?;

        Ok(())
    }

    pub async fn record_vizier_event(&self, event: VizierEventRecord) -> Result<()> {
        let client = self.pool.get().await.context("getting postgres connection")?;
        self.ensure_vizier_schema(&client)
            .await
            .context("ensuring vizier schema")?;

        let event_id = Uuid::new_v4();
        let payload = Json(event.payload);
        let producer = "ci-applet-vizier";

        let params: &[&(dyn ToSql + Sync)] = &[
            &event_id,
            &event.user_id,
            &event.device_id,
            &event.session_id,
            &producer,
            &event.event_kind,
            &event.observed_at,
            &event.monotonic_ms,
            &payload,
            &event.focus_app,
            &event.focus_title,
            &event.cwd,
            &event.fs_event_count,
        ];

        client
            .execute(
                "INSERT INTO ci.vizier_events (
                    event_id,
                    user_id,
                    device_id,
                    session_id,
                    producer,
                    event_kind,
                    observed_at,
                    monotonic_ms,
                    payload,
                    focus_app,
                    focus_title,
                    cwd,
                    fs_event_count
                ) VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13
                )",
                params,
            )
            .await
            .context("executing vizier event insert")?;

        Ok(())
    }

    async fn ensure_vizier_schema(&self, client: &Client<NoTls>) -> Result<()> {
        if self.vizier_schema_ready.load(Ordering::SeqCst) {
            return Ok(());
        }

        client
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS ci;
                CREATE TABLE IF NOT EXISTS ci.vizier_events (
                    event_id uuid PRIMARY KEY,
                    user_id uuid NOT NULL,
                    device_id uuid NOT NULL,
                    session_id uuid NOT NULL,
                    producer text NOT NULL,
                    event_kind text NOT NULL CHECK (event_kind IN ('snapshot', 'patch')),
                    observed_at timestamptz NOT NULL,
                    monotonic_ms bigint NOT NULL,
                    payload jsonb NOT NULL,
                    focus_app text NULL,
                    focus_title text NULL,
                    cwd text NULL,
                    fs_event_count integer NOT NULL DEFAULT 0,
                    created_at timestamptz NOT NULL DEFAULT now()
                );
                CREATE INDEX IF NOT EXISTS vizier_events_user_time_idx
                    ON ci.vizier_events (user_id, observed_at DESC);
                CREATE INDEX IF NOT EXISTS vizier_events_session_time_idx
                    ON ci.vizier_events (session_id, observed_at DESC);
                CREATE INDEX IF NOT EXISTS vizier_events_payload_gin_idx
                    ON ci.vizier_events USING GIN (payload);",
            )
            .await
            .context("creating vizier schema objects")?;

        self.vizier_schema_ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn insert_file_record(
        &self,
        client: &Client<NoTls>,
        metadata: &ScreenshotRecord,
    ) -> Result<Uuid> {
        let timestamp = metadata.captured_at;
        let file_ext = metadata
            .filename
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_string());
        let ext_ref = file_ext.as_deref();
        let bucket = Some(self.config.minio.bucket.clone());

        let params: &[&(dyn ToSql + Sync)] = &[
            &metadata.user_id,
            &metadata.remote_path,
            &metadata.filename,
            &ext_ref,
            &bucket,
            &"cloud",
            &"minio",
            &metadata.device_id,
            &metadata.file_size_bytes,
            &"image/png",
            &"image",
            &timestamp,
            &timestamp,
            &timestamp,
        ];

        let row = client
            .query_one(
                "INSERT INTO ci.files (
                    user_id,
                    file_path,
                    filename,
                    file_extension,
                    bucket,
                    storage_type,
                    storage_provider,
                    device_id,
                    file_size_bytes,
                    mime_type,
                    content_type,
                    modified_at_source,
                    discovered_at,
                    last_scanned_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
                RETURNING file_id",
                params,
            )
            .await
            .context("executing files insert")?;
        Ok(row.get("file_id"))
    }

    async fn insert_screen_capture(
        &self,
        client: &Client<NoTls>,
        metadata: &ScreenshotRecord,
        analysis: &AnalysisResult,
        _file_id: Uuid,
    ) -> Result<u64> {
        let ui_json = Json(json!(analysis.ui_elements));
        let apps_json = Json(json!(analysis.applications));
        let redactions = Json(json!([]));

        let params: &[&(dyn ToSql + Sync)] = &[
            &metadata.captured_at,
            &metadata.user_id,
            &metadata.device_id,
            &metadata.remote_path,
            &Option::<String>::None,
            &analysis.ocr_text,
            &ui_json,
            &apps_json,
            &analysis.activity_summary,
            &analysis.primary_application,
            &analysis.activity_category,
            &analysis.contains_sensitive,
            &analysis.contains_pii,
            &redactions,
        ];

        client
            .execute(
                "INSERT INTO ci.screen_captures (
                    time,
                    user_id,
                    device_id,
                    image_path,
                    thumbnail_path,
                    ocr_text,
                    ui_elements,
                    applications,
                    activity_summary,
                    primary_application,
                    activity_category,
                    contains_sensitive,
                    contains_pii,
                    redaction_regions
                ) VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
                params,
            )
            .await
            .context("executing screen capture insert")
    }
}
