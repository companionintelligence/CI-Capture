use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const APP_QUALIFIER: &str = "com.compint";
/// Drives the on-disk config/data directory via `directories::ProjectDirs`.
///
/// Renamed from `CI-Applet` in the Companion Capture rebrand. This is a
/// deliberate clean break: existing v0.1.x installs keep their old directory
/// (`…/com.compint.com.compint.CI-Applet`) untouched but unread, so the app
/// starts fresh — no credentials, no watched folders, no device id. The bundle
/// identifier changed in the same release, which independently resets macOS
/// TCC grants, so a partial migration would have left users half-configured
/// and harder to diagnose than a clean first run.
const APP_NAME: &str = "Companion Capture";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MinioConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub use_ssl: bool,
}

impl Default for MinioConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:9000".to_string(),
            bucket: "ci-screens".to_string(),
            region: "us-east-1".to_string(),
            access_key: "minio".to_string(),
            secret_key: "minio123".to_string(),
            use_ssl: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub require_ssl: bool,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "postgres".to_string(),
            database: "ci".to_string(),
            require_ssl: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisConfig {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            // Empty by default: a non-empty endpoint is the applet's signal
            // that it should analyse screenshots locally (and tell CI-Server
            // to skip its own Ollama description). Defaulting to a localhost
            // URL where nothing listens made fresh installs suppress the
            // server description AND fail locally → zero-metadata frames. When
            // empty, CI-Server describes captures itself.
            endpoint: String::new(),
            api_key: "lm-studio".to_string(),
            model: "lmstudio-community/vision-large".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct IdentityConfig {
    pub user_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub screenshot_interval_secs: u64,
    pub diff_threshold_percent: f32,
    pub storage_dir: PathBuf,
    /// Prune locally stored screenshots older than this many days, whether or
    /// not they were uploaded. 0 disables age pruning. Uploaded frames are
    /// deleted immediately on server ack — this cap bounds the *unsent*
    /// backlog (and the legacy no-server path) so full-resolution screen
    /// content never accumulates unbounded on disk.
    #[serde(default = "default_retention_max_age_days")]
    pub retention_max_age_days: u64,
    /// Cap the total size of locally stored screenshots, pruning oldest
    /// first. 0 disables the size cap.
    #[serde(default = "default_retention_max_total_mb")]
    pub retention_max_total_mb: u64,
}

fn default_retention_max_age_days() -> u64 {
    14
}

fn default_retention_max_total_mb() -> u64 {
    2048
}

impl Default for CaptureConfig {
    fn default() -> Self {
        let default_dir = default_storage_dir().unwrap_or_else(|_| std::env::temp_dir());
        Self {
            screenshot_interval_secs: 300,
            diff_threshold_percent: 10.0,
            storage_dir: default_dir,
            retention_max_age_days: default_retention_max_age_days(),
            retention_max_total_mb: default_retention_max_total_mb(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VizierConfig {
    pub enabled: bool,
    pub watch_path: Option<PathBuf>,
    pub all_connections: bool,
}

impl Default for VizierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_path: None,
            all_connections: false,
        }
    }
}

/// File-activity tracking lane. Watches a user-selected set of folders,
/// fingerprints changed files, converts the ones it can to Markdown, and
/// streams the result to CI-Server so the user's AI can remember them.
///
/// Capture is **off until `watched_paths` is non-empty** — the applet never
/// watches anything the user didn't explicitly add.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FileTrackerConfig {
    pub enabled: bool,
    /// Folders to watch recursively. Empty = lane is a no-op.
    pub watched_paths: Vec<PathBuf>,
    /// Extra user-defined ignore globs, stacked on top of the built-in
    /// defaults + non-disableable sensitive list.
    pub ignore_globs: Vec<String>,
    /// Per-file size cap in bytes; larger files record an event but no hash.
    pub max_file_bytes: u64,
    /// When true, converted Markdown content is included in the synced
    /// payload. When false, only metadata (path/size/hash) is sent.
    pub send_content: bool,
    /// Max bytes of converted Markdown to embed per event (keeps ingest
    /// payloads bounded; the full blob upload is a separate concern).
    pub max_content_bytes: usize,
}

impl Default for FileTrackerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watched_paths: Vec::new(),
            ignore_globs: Vec::new(),
            max_file_bytes: 100 * 1024 * 1024,
            send_content: true,
            max_content_bytes: 256 * 1024,
        }
    }
}

/// BlueBubbles lane — prototype (see `crate::bluebubbles`). Off by default
/// and, like `FileTrackerConfig`, effectively a no-op until a host and
/// password are actually entered; `bluebubbles::is_runnable` is the single
/// source of truth for "should this lane poll."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BlueBubblesConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub use_https: bool,
    /// BlueBubbles' own server password — a full-control credential for the
    /// configured Mac. Stored in plaintext on disk, consistent with this
    /// file's existing precedent for local service credentials (Postgres,
    /// MinIO, above).
    pub password: String,
    pub poll_interval_secs: u64,
}

impl Default for BlueBubblesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 1234,
            use_https: false,
            password: String::new(),
            poll_interval_secs: 30,
        }
    }
}

/// Local iMessage/SMS lane — prototype (see `crate::imessage`). Off by
/// default; unlike `BlueBubblesConfig` there is no host/port/password to
/// enter, since it reads this Mac's own `~/Library/Messages/chat.db`
/// directly, gated by Full Disk Access rather than a saved credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ImessageConfig {
    pub enabled: bool,
    pub poll_interval_secs: u64,
}

impl Default for ImessageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: 30,
        }
    }
}

/// The persisted applet configuration.
///
/// Every config struct in this module — this one and each section — carries a
/// struct-level `#[serde(default)]`. `load_or_default` propagates parse errors
/// and `main` treats them as fatal, so *any* missing key would otherwise stop
/// the tray from starting, leaving no UI to repair it from. Struct-level rather
/// than field-level is deliberate: a field-level default only rescues a wholly
/// absent section, while users are explicitly pointed at this file to hand-edit
/// and will delete individual keys. It also means a section can be retired in a
/// future release without bricking installs that still have it on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub capture: CaptureConfig,
    #[serde(default)]
    pub minio: MinioConfig,
    #[serde(default)]
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub analysis: AnalysisConfig,
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub vizier: VizierConfig,
    #[serde(default)]
    pub file_tracker: FileTrackerConfig,
    #[serde(default)]
    pub bluebubbles: BlueBubblesConfig,
    #[serde(default)]
    pub imessage: ImessageConfig,
    #[serde(default)]
    pub oidc: OidcConfig,
    /// Tokens + claims captured from the last successful portal sign-in.
    #[serde(default)]
    pub oidc_session: Option<OidcSession>,
    /// Set once the first-run onboarding wizard has been completed (or skipped).
    /// Drives whether the wizard auto-opens on launch.
    #[serde(default)]
    pub onboarding_completed: bool,
    /// Language for the UI chrome. Defaults to [`Language::System`], which
    /// resolves against the host; an explicit choice is honoured from then on.
    /// See `crate::locale`.
    #[serde(default)]
    pub ui_language: crate::locale::Language,
    /// Send scrubbed panic reports to Sentry.
    ///
    /// Defaults to **`true`**: the fleet's posture is opt-out (owner decision,
    /// 2026-08-03), so reporting is on wherever a DSN was compiled in and the
    /// user turns it off. This is still the one path that sends anything to a
    /// machine the user does not own, which is why `crate::telemetry` scrubs
    /// every event and why a build with no DSN — every local build — sends
    /// nothing regardless of this flag.
    ///
    /// The field default is spelled out rather than derived because
    /// `bool::default()` is `false`; see the `Default` impl below, which exists
    /// for this field alone.
    #[serde(default = "crash_reporting_default")]
    pub crash_reporting: bool,
}

/// `true` — see [`AppConfig::crash_reporting`].
fn crash_reporting_default() -> bool {
    true
}

impl Default for AppConfig {
    /// Hand-written, not derived, for one field: `crash_reporting` must default
    /// to `true` and `#[derive(Default)]` would quietly make it `false` — which
    /// would matter twice over, because `load_or_default` *persists* this value
    /// when there is no config file yet. Every other field is its own `Default`,
    /// and a field added later fails to compile here rather than defaulting by
    /// accident.
    fn default() -> Self {
        Self {
            capture: CaptureConfig::default(),
            minio: MinioConfig::default(),
            postgres: PostgresConfig::default(),
            analysis: AnalysisConfig::default(),
            identity: IdentityConfig::default(),
            server: ServerConfig::default(),
            vizier: VizierConfig::default(),
            file_tracker: FileTrackerConfig::default(),
            bluebubbles: BlueBubblesConfig::default(),
            imessage: ImessageConfig::default(),
            oidc: OidcConfig::default(),
            oidc_session: None,
            onboarding_completed: false,
            ui_language: crate::locale::Language::default(),
            crash_reporting: crash_reporting_default(),
        }
    }
}

/// OpenID Connect client config for portal sign-in (Authorization Code +
/// PKCE). Discovery-driven, so only the issuer + client id usually change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OidcConfig {
    /// Issuer base; the applet fetches `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// Registered public client id for the applet.
    pub client_id: String,
    /// Space-delimited scopes (`offline_access` requests a refresh token).
    pub scopes: String,
    /// Loopback redirect the portal sends the code back to. Must be one of the
    /// client's registered redirect URIs and include a port.
    pub redirect_uri: String,
}

/// Production CI-Portal sign-in URL.
///
/// A real default rather than a blank field, so a fresh install — including a
/// headless `--login` with no config at all — signs in with zero typing.
/// `hub.ci.computer` is production; `hub.companionintelligence.com`, which
/// several repos still default to, is the dev worker per CI-Portal's
/// `wrangler.jsonc`. Self-hosters override `oidc.issuer`.
pub const DEFAULT_PORTAL_ISSUER: &str = "https://hub.ci.computer/api/auth";

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer: DEFAULT_PORTAL_ISSUER.to_string(),
            client_id: "ci-applet".to_string(),
            scopes: "openid profile email offline_access".to_string(),
            redirect_uri: "http://127.0.0.1:0/callback".to_string(),
        }
    }
}

/// Tokens + identity from the last portal sign-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OidcSession {
    pub id_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub subject: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    pub portal_url: String,
    #[serde(default)]
    pub applet_auth_token: Option<String>,
    /// How `portal_url` came to hold its current value.
    ///
    /// Portal discovery may replace an address it set itself, but must never
    /// replace one the user typed — someone who deliberately pointed this
    /// machine at a specific server should not have it moved out from under them
    /// on the next sign-in.
    #[serde(default)]
    pub url_source: ServerUrlSource,
    /// Hub device slug for the Memory this Capture install chose (`deviceSlug`
    /// from Portal `/me/apps`). Many Capture installs may share the same slug.
    #[serde(default)]
    pub bound_device_slug: Option<String>,
    /// Display name for [`Self::bound_device_slug`] (tray / Settings).
    #[serde(default)]
    pub bound_device_name: Option<String>,
}

/// Provenance of [`ServerConfig::portal_url`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerUrlSource {
    /// Typed by the user, or inherited from a config written before this field
    /// existed — treated as manual so an upgrade cannot silently re-point an
    /// existing install.
    #[default]
    Manual,
    /// Resolved from CI-Portal's app discovery.
    Discovered,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            portal_url: String::new(),
            applet_auth_token: None,
            url_source: ServerUrlSource::Manual,
            bound_device_slug: None,
            bound_device_name: None,
        }
    }
}

pub struct ConfigManager {
    path: PathBuf,
}

impl ConfigManager {
    pub fn new() -> Result<Self> {
        let dir = ensure_config_dir()?;
        Ok(Self {
            path: dir.join("config.json"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_or_default(&self) -> Result<AppConfig> {
        if self.path.exists() {
            let data = fs::read_to_string(&self.path)
                .with_context(|| format!("reading config at {}", self.path.display()))?;
            let config: AppConfig =
                serde_json::from_str(&data).with_context(|| "parsing config".to_string())?;
            Ok(config)
        } else {
            let config = AppConfig::default();
            self.save(&config)?;
            Ok(config)
        }
    }

    /// Atomic save: write to a sibling temp file, then rename over
    /// `config.json`. Three writers share this file (tray process, settings
    /// child process, token-refresh thread) — the rename guarantees a reader
    /// never sees torn JSON, whichever writer lands last.
    pub fn save(&self, config: &AppConfig) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let serialized = serde_json::to_string_pretty(config)?;
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", std::process::id()));
        fs::write(&tmp, serialized)
            .with_context(|| format!("writing config temp file {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| {
            let _ = fs::remove_file(&tmp);
            format!("committing config to {}", self.path.display())
        })?;
        Ok(())
    }
}

fn ensure_config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating project directory")?;
    let dir = dirs.config_dir();
    fs::create_dir_all(dir)
        .with_context(|| format!("creating config directory {}", dir.display()))?;
    Ok(dir.to_path_buf())
}

/// Path to the durable file-event queue database (SQLite). Lives next to
/// the other applet data so it survives restarts and network outages.
pub fn queue_db_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating data directory")?;
    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    Ok(data_dir.join("file-tracker-queue.db"))
}

/// Path to the BlueBubbles lane's sync cursor (plain JSON, not the SQLite
/// queue file_tracker uses — see `crate::bluebubbles` for why).
pub fn bluebubbles_cursor_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating data directory")?;
    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    Ok(data_dir.join("bluebubbles-cursor.json"))
}

/// Path to the local iMessage lane's sync cursor.
pub fn imessage_cursor_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating data directory")?;
    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    Ok(data_dir.join("imessage-cursor.json"))
}

pub fn default_storage_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating storage directory")?;
    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let screenshot_dir = data_dir.join("screenshots");
    fs::create_dir_all(&screenshot_dir)
        .with_context(|| format!("creating screenshot dir {}", screenshot_dir.display()))?;
    Ok(screenshot_dir)
}

#[cfg(test)]
mod analysis_default_tests {
    use super::AnalysisConfig;

    #[test]
    fn default_endpoint_is_empty_so_server_analyses() {
        // A non-empty default made fresh installs suppress the server
        // description then fail locally → zero-metadata frames.
        assert!(
            AnalysisConfig::default().endpoint.is_empty(),
            "default analysis endpoint must be empty (server-side analysis)"
        );
    }
}

#[cfg(test)]
mod rebrand_tests {
    use super::*;

    /// A config.json missing whole sections must still load. `load_or_default`
    /// propagates parse errors and `main` treats them as fatal, so without
    /// `#[serde(default)]` on every section a hand-edited or forward-migrated
    /// file would stop the tray from starting with no UI left to repair it from.
    #[test]
    fn config_loads_with_every_section_absent() {
        let cfg: AppConfig = serde_json::from_str("{}").expect("empty object must load");
        assert_eq!(cfg.capture.screenshot_interval_secs, 300);
        assert_eq!(cfg.postgres.port, 5432);
        assert!(cfg.server.portal_url.is_empty());
        assert!(!cfg.onboarding_completed);
    }

    /// Retiring the minio/postgres sections must not brick existing installs.
    #[test]
    fn config_loads_with_infra_sections_removed() {
        let json = r#"{
            "server": { "portal_url": "http://core-1:8642" },
            "file_tracker": { "enabled": true, "watched_paths": [] }
        }"#;
        let cfg: AppConfig = serde_json::from_str(json).expect("must load");
        assert_eq!(cfg.server.portal_url, "http://core-1:8642");
        assert_eq!(cfg.minio.bucket, "ci-screens");
    }

    /// The posture is opt-out, so both paths that produce a config without an
    /// explicit choice must produce `true`.
    ///
    /// Two paths, not one, and they are easy to get out of step: `#[serde(default)]`
    /// at the *field* level would use `bool::default()` (false) and quietly
    /// override the container default, while `#[derive(Default)]` would make
    /// `load_or_default` **persist** `false` into a fresh install's config.json.
    #[test]
    fn crash_reporting_defaults_to_on() {
        assert!(AppConfig::default().crash_reporting);
        let cfg: AppConfig = serde_json::from_str("{}").expect("empty object must load");
        assert!(
            cfg.crash_reporting,
            "a config with no key must default to on"
        );
    }

    /// …and an explicit `false` is honoured, which is the whole point of a
    /// toggle. A user who turned it off must stay off across upgrades.
    #[test]
    fn an_explicit_crash_reporting_choice_wins() {
        let off: AppConfig = serde_json::from_str(r#"{ "crash_reporting": false }"#).unwrap();
        assert!(!off.crash_reporting);
        let on: AppConfig = serde_json::from_str(r#"{ "crash_reporting": true }"#).unwrap();
        assert!(on.crash_reporting);
    }

    /// Unknown keys are ignored rather than rejected, so a config written by a
    /// newer build still loads in an older one.
    #[test]
    fn config_ignores_unknown_keys() {
        let json = r#"{ "server": { "portal_url": "http://x:1" }, "future_section": { "a": 1 } }"#;
        let cfg: AppConfig = serde_json::from_str(json).expect("must ignore unknown keys");
        assert_eq!(cfg.server.portal_url, "http://x:1");
    }
}
