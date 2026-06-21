// The screenshot-recording helpers here are only reached by the legacy
// MinIO path (`--features legacy-minio`); they're dead code in the default
// build. The vizier snapshot path is always used.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use deadpool_postgres::{Client, Config, ManagerConfig, Pool, RecyclingMethod};
use serde_json::{Value, json};
use tokio_postgres::{
    NoTls,
    types::{Json, ToSql},
};
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
    screen_capture_schema_ready: Arc<AtomicBool>,
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
            screen_capture_schema_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn record_capture(
        &self,
        metadata: ScreenshotRecord,
        analysis: &AnalysisResult,
    ) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("getting postgres connection")?;
        self.ensure_screen_capture_schema(&client)
            .await
            .context("ensuring screen capture schema")?;

        let file_id = self
            .insert_file_record(&client, &metadata)
            .await
            .context("inserting file metadata")?;

        self.insert_screen_capture(&client, &metadata, analysis, file_id)
            .await
            .context("inserting screen capture")?;

        Ok(())
    }

    async fn ensure_screen_capture_schema(&self, client: &Client<NoTls>) -> Result<()> {
        if self.screen_capture_schema_ready.load(Ordering::SeqCst) {
            return Ok(());
        }

        client
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS ci;
                CREATE TABLE IF NOT EXISTS ci.screen_captures (
                    time timestamptz NOT NULL,
                    user_id uuid NOT NULL,
                    device_id uuid NOT NULL,
                    image_path text NOT NULL,
                    thumbnail_path text NULL,
                    ocr_text text NULL,
                    ui_elements jsonb NOT NULL DEFAULT '[]'::jsonb,
                    applications jsonb NOT NULL DEFAULT '[]'::jsonb,
                    activity_summary text NULL,
                    primary_application text NULL,
                    activity_category text NULL,
                    contains_sensitive boolean NOT NULL DEFAULT false,
                    contains_pii boolean NOT NULL DEFAULT false,
                    redacted boolean NOT NULL DEFAULT false,
                    redaction_regions jsonb NOT NULL DEFAULT '[]'::jsonb,
                    PRIMARY KEY (time, user_id, device_id)
                );
                CREATE INDEX IF NOT EXISTS screen_captures_user_time_idx
                    ON ci.screen_captures (user_id, time DESC);
                CREATE INDEX IF NOT EXISTS screen_captures_device_time_idx
                    ON ci.screen_captures (device_id, time DESC);",
            )
            .await
            .context("creating screen capture schema objects")?;

        self.screen_capture_schema_ready
            .store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn record_vizier_event(&self, event: VizierEventRecord) -> Result<()> {
        let client = self
            .pool
            .get()
            .await
            .context("getting postgres connection")?;

        let snapshot_id = Uuid::new_v4();
        let payload = Json(event.payload);
        let producer = "ci-applet-vizier";

        let params: &[&(dyn ToSql + Sync)] = &[
            &snapshot_id,
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
                "INSERT INTO ci.user_snapshots (
                    snapshot_id,
                    observed_at,
                    user_id,
                    device_id,
                    session_id,
                    producer,
                    snapshot_kind,
                    monotonic_ms,
                    payload,
                    focus_app,
                    focus_title,
                    cwd,
                    fs_event_count
                ) VALUES (
                    $1,$7,$2,$3,$4,$5,$6,$8,$9,$10,$11,$12,$13
                )",
                params,
            )
            .await
            .context("executing user snapshot insert")?;

        Ok(())
    }

    async fn insert_file_record(
        &self,
        client: &Client<NoTls>,
        metadata: &ScreenshotRecord,
    ) -> Result<Uuid> {
        let timestamp = metadata.captured_at;
        let file_id = Uuid::new_v4();
        let bucket = Some(self.config.minio.bucket.clone());
        let object_key = Some(Uuid::new_v4());
        let variant = "original";

        let params: &[&(dyn ToSql + Sync)] = &[
            &file_id,
            &metadata.filename,
            &"image/png",
            &metadata.file_size_bytes,
            &variant,
            &bucket,
            &object_key,
            &metadata.user_id,
            &Some(metadata.remote_path.clone()),
            &timestamp,
            &timestamp,
            &timestamp,
            &timestamp,
        ];

        let row = client
            .query_one(
                "INSERT INTO ci.files (
                    id,
                    filename,
                    content_type,
                    size,
                    variant,
                    bucket,
                    key,
                    user_id,
                    external_id,
                    external_created_at,
                    external_updated_at,
                    created_at,
                    updated_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                RETURNING id",
                params,
            )
            .await
            .context("executing files insert")?;
        Ok(row.get("id"))
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
