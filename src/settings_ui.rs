use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context, Result, anyhow};
use eframe::{NativeOptions, egui};
use parking_lot::Mutex;
use reqwest::Url;
use uuid::Uuid;

use crate::config::{AppConfig, ConfigManager, MinioConfig, PostgresConfig};
use crate::connection_test;
use crate::server_common::normalize_server_url;
use crate::theme;
use crate::tokens;

/// Result of an inline connection test for one tab.
#[derive(Clone)]
enum TestState {
    Running,
    Ok(String),
    Err(String),
}

pub fn run_settings_ui() -> Result<()> {
    let manager = ConfigManager::new().context("building config manager")?;
    let config = manager.load_or_default().context("loading configuration")?;

    // This window is its own process (`--settings-ui`), and `main` applies the
    // language only on the tray path — the `--settings-ui` branch returns long
    // before it. Without this call every string below renders in rust-i18n's
    // default locale, so the whole catalogue would be dead weight here. Must
    // precede the first `t!`, which is the window title on the next line.
    crate::locale::apply(config.ui_language);

    let manager = Arc::new(manager);
    let shared_saved = Arc::new(Mutex::new(false));
    let base_config = Arc::new(Mutex::new(config.clone()));

    let manager_clone = manager.clone();
    let saved_clone = shared_saved.clone();
    let base_clone = base_config.clone();

    let mut native_options = NativeOptions::default();
    native_options.centered = true;
    native_options.viewport = egui::ViewportBuilder::default()
        .with_title(rust_i18n::t!("settings.window_title").to_string())
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([820.0, 600.0]);

    eframe::run_native(
        // Not the visible title (`with_title` above owns that) — eframe uses
        // this as the app id, and it names the persistence directory. Keep it
        // stable and untranslated.
        "Companion Capture Settings",
        native_options,
        Box::new(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            crate::theme::apply_visuals(&cc.egui_ctx);
            let logo = crate::theme::load_logo(&cc.egui_ctx);
            Ok(Box::new(SettingsApp::new(
                manager_clone.clone(),
                base_clone.clone(),
                saved_clone.clone(),
                logo,
            )))
        }),
    )
    .map_err(|err| anyhow!("failed to run settings UI: {err}"))?;

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsFormState {
    pub user_id: String,
    pub device_id: String,
    pub portal_url: String,
    /// When set with a list pick, Save writes `url_source = Discovered`.
    pub bound_device_slug: String,
    pub bound_device_name: String,
    /// True after the user picks a Memory from Portal discovery (not typed).
    pub memory_from_discovery: bool,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_scopes: String,
    pub oidc_redirect_uri: String,
    pub applet_auth_token: String,
    pub screenshot_interval_secs: u64,
    pub diff_threshold_percent: f32,
    /// Folders the file-tracking lane watches. The lane stays idle while this
    /// is empty (`file_tracker::is_runnable`), so this is the control that
    /// actually arms it.
    pub watched_paths: Vec<PathBuf>,
    pub file_tracking_enabled: bool,
    pub minio_endpoint: String,
    pub minio_bucket: String,
    pub minio_region: String,
    pub minio_access_key: String,
    pub minio_secret_key: String,
    pub minio_use_ssl: bool,
    pub postgres_host: String,
    pub postgres_port: u16,
    pub postgres_user: String,
    pub postgres_password: String,
    pub postgres_database: String,
    pub postgres_require_ssl: bool,
    pub analysis_endpoint: String,
    pub analysis_api_key: String,
    pub analysis_model: String,
    /// UI language. Read on the next window launch, not applied live — see the
    /// picker in [`render_advanced_tab`] and `crate::locale`.
    pub ui_language: crate::locale::Language,
    auth_mode: AuthMode,
}

impl From<&AppConfig> for SettingsFormState {
    fn from(config: &AppConfig) -> Self {
        // A present OIDC session means the applet_auth_token holds an
        // id_token (not a pasted key), so infer OIDC first — otherwise the
        // token check below would mislabel a signed-in user as "API key" and
        // render their raw JWT in the key field.
        let auth_mode = if config.oidc_session.is_some() {
            AuthMode::Oidc
        } else if config
            .server
            .applet_auth_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
        {
            AuthMode::ApiKey
        } else if !config.oidc.issuer.trim().is_empty() {
            AuthMode::Oidc
        } else {
            AuthMode::ApiKey
        };

        Self {
            user_id: config
                .identity
                .user_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            device_id: config
                .identity
                .device_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            portal_url: config.server.portal_url.clone(),
            bound_device_slug: config
                .server
                .bound_device_slug
                .clone()
                .unwrap_or_default(),
            bound_device_name: config
                .server
                .bound_device_name
                .clone()
                .unwrap_or_default(),
            memory_from_discovery: config.server.url_source
                == crate::config::ServerUrlSource::Discovered,
            oidc_issuer: config.oidc.issuer.clone(),
            oidc_client_id: config.oidc.client_id.clone(),
            oidc_scopes: config.oidc.scopes.clone(),
            oidc_redirect_uri: config.oidc.redirect_uri.clone(),
            applet_auth_token: config.server.applet_auth_token.clone().unwrap_or_default(),
            screenshot_interval_secs: config.capture.screenshot_interval_secs,
            diff_threshold_percent: config.capture.diff_threshold_percent,
            watched_paths: config.file_tracker.watched_paths.clone(),
            file_tracking_enabled: config.file_tracker.enabled,
            minio_endpoint: config.minio.endpoint.clone(),
            minio_bucket: config.minio.bucket.clone(),
            minio_region: config.minio.region.clone(),
            minio_access_key: config.minio.access_key.clone(),
            minio_secret_key: config.minio.secret_key.clone(),
            minio_use_ssl: config.minio.use_ssl,
            postgres_host: config.postgres.host.clone(),
            postgres_port: config.postgres.port,
            postgres_user: config.postgres.user.clone(),
            postgres_password: config.postgres.password.clone(),
            postgres_database: config.postgres.database.clone(),
            postgres_require_ssl: config.postgres.require_ssl,
            analysis_endpoint: config.analysis.endpoint.clone(),
            analysis_api_key: config.analysis.api_key.clone(),
            analysis_model: config.analysis.model.clone(),
            ui_language: config.ui_language,
            auth_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Oidc,
}

/// The settings sections.
///
/// Deliberately short. A laptop running Companion Capture holds no infrastructure
/// of its own: storage and inference live on the Companion Memory server, over LAN or
/// Tailscale. So there is nothing here to configure for either — the old Storage
/// (MinIO) and Database (Postgres) tabs are compiled out entirely unless their
/// legacy feature is enabled, rather than hidden behind a runtime toggle. Hiding
/// them was worse than useless: the Storage tab configured a module that is not
/// even compiled into a default build, so every field in it was inert.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsTab {
    Connection,
    Capture,
    Advanced,
    #[cfg(feature = "legacy-minio")]
    Minio,
    #[cfg(feature = "legacy-postgres")]
    Postgres,
}

impl SettingsTab {
    /// Localised, so this is a `String` rather than the `&'static str` a plain
    /// match would give. Doubles as the page heading for each tab.
    fn label(&self) -> String {
        match self {
            SettingsTab::Connection => rust_i18n::t!("settings.tab.connection"),
            SettingsTab::Capture => rust_i18n::t!("settings.tab.capture"),
            SettingsTab::Advanced => rust_i18n::t!("settings.tab.advanced"),
            #[cfg(feature = "legacy-minio")]
            SettingsTab::Minio => rust_i18n::t!("settings.tab.storage"),
            #[cfg(feature = "legacy-postgres")]
            SettingsTab::Postgres => rust_i18n::t!("settings.tab.database"),
        }
        .to_string()
    }

    /// Short descriptor shown under the nav label.
    fn subtitle(&self) -> String {
        match self {
            SettingsTab::Connection => rust_i18n::t!("settings.tab.connection.subtitle"),
            SettingsTab::Capture => rust_i18n::t!("settings.tab.capture.subtitle"),
            SettingsTab::Advanced => rust_i18n::t!("settings.tab.advanced.subtitle"),
            #[cfg(feature = "legacy-minio")]
            SettingsTab::Minio => rust_i18n::t!("settings.tab.storage.subtitle"),
            #[cfg(feature = "legacy-postgres")]
            SettingsTab::Postgres => rust_i18n::t!("settings.tab.database.subtitle"),
        }
        .to_string()
    }

    /// Accent colour for the nav chip.
    fn accent(&self) -> egui::Color32 {
        match self {
            SettingsTab::Connection => theme::TEAL(),
            SettingsTab::Capture => theme::LANE_ACTIVITY(),
            SettingsTab::Advanced => theme::MUTED(),
            #[cfg(feature = "legacy-minio")]
            SettingsTab::Minio => theme::AMBER(),
            #[cfg(feature = "legacy-postgres")]
            SettingsTab::Postgres => theme::AMBER(),
        }
    }

    /// Whether this tab configures a remote service that can be test-probed.
    fn has_connection(&self) -> bool {
        !matches!(self, SettingsTab::Capture | SettingsTab::Advanced)
    }

    fn all() -> Vec<SettingsTab> {
        vec![
            SettingsTab::Connection,
            SettingsTab::Capture,
            SettingsTab::Advanced,
            #[cfg(feature = "legacy-minio")]
            SettingsTab::Minio,
            #[cfg(feature = "legacy-postgres")]
            SettingsTab::Postgres,
        ]
    }
}

struct SettingsApp {
    manager: Arc<ConfigManager>,
    base_config: Arc<Mutex<AppConfig>>,
    form: SettingsFormState,
    current_tab: SettingsTab,
    error: Option<String>,
    saved_flag: Arc<Mutex<bool>>,
    logo: egui::TextureHandle,
    runtime: tokio::runtime::Runtime,
    test_tx: Sender<(SettingsTab, Result<String, String>)>,
    test_rx: Receiver<(SettingsTab, Result<String, String>)>,
    tests: HashMap<SettingsTab, TestState>,
    discover_tx: Sender<Result<Vec<crate::portal_apps::PortalApp>, String>>,
    discover_rx: Receiver<Result<Vec<crate::portal_apps::PortalApp>, String>>,
    memory_choices: Vec<crate::portal_apps::PortalApp>,
    memory_status: Option<String>,
    discovering: bool,
    /// One-shot latch for [`theme::center_once`].
    centered: bool,
}

impl SettingsApp {
    fn new(
        manager: Arc<ConfigManager>,
        base_config: Arc<Mutex<AppConfig>>,
        saved_flag: Arc<Mutex<bool>>,
        logo: egui::TextureHandle,
    ) -> Self {
        let form = {
            let cfg = base_config.lock();
            SettingsFormState::from(&*cfg)
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to create settings runtime");
        let (test_tx, test_rx) = channel();
        let (discover_tx, discover_rx) = channel();
        Self {
            manager,
            base_config,
            form,
            current_tab: SettingsTab::Connection,
            error: None,
            saved_flag,
            logo,
            runtime,
            test_tx,
            test_rx,
            tests: HashMap::new(),
            discover_tx,
            discover_rx,
            memory_choices: Vec::new(),
            memory_status: None,
            discovering: false,
            centered: false,
        }
    }

    /// Build an `AppConfig` from the current form, layered onto the last saved
    /// config. Shared by `save` and the connection tests so a test always
    /// probes exactly what the form shows.
    fn build_config(&self) -> AppConfig {
        let mut c = self.base_config.lock().clone();
        // `validate()` gates Save on the same call, so the fallback here is
        // only reached by the connection test probing a half-typed field —
        // keep the raw text in that case so the test reports what the user is
        // actually looking at.
        let typed = normalize_server_url(self.form.portal_url.trim())
            .unwrap_or_else(|_| self.form.portal_url.trim().to_string());
        if self.form.memory_from_discovery {
            c.server.url_source = crate::config::ServerUrlSource::Discovered;
            c.server.bound_device_slug = Some(self.form.bound_device_slug.trim().to_string())
                .filter(|s| !s.is_empty());
            c.server.bound_device_name = Some(self.form.bound_device_name.trim().to_string())
                .filter(|s| !s.is_empty());
        } else if typed != c.server.portal_url {
            // Typed address — Portal discovery must not move it later.
            c.server.url_source = crate::config::ServerUrlSource::Manual;
            c.server.bound_device_slug = None;
            c.server.bound_device_name = None;
        }
        c.server.portal_url = typed;
        // The tray login + token-refresh flow owns the OIDC session, the
        // id_token bearer, and the registered device id — and runs in a
        // separate process that may have written config.json while this
        // settings window was open. Re-read those fields from disk so Save
        // merges rather than clobbers a fresh sign-in (the form never edits
        // them directly).
        let live = ConfigManager::new()
            .ok()
            .and_then(|m| m.load_or_default().ok());
        if let Some(live) = &live {
            c.oidc_session = live.oidc_session.clone();
            c.identity.device_id = live.identity.device_id;
        }
        let auth_token = self.form.applet_auth_token.trim();
        c.server.applet_auth_token = match self.form.auth_mode {
            // API-key mode: the field IS the credential.
            AuthMode::ApiKey => (!auth_token.is_empty()).then(|| auth_token.to_string()),
            // OIDC mode: preserve the id_token bearer managed by login/refresh
            // instead of wiping it on an unrelated settings save.
            AuthMode::Oidc => live.and_then(|l| l.server.applet_auth_token),
        };
        c.oidc.issuer = self.form.oidc_issuer.trim().to_string();
        c.oidc.client_id = self.form.oidc_client_id.trim().to_string();
        c.oidc.scopes = self.form.oidc_scopes.trim().to_string();
        c.oidc.redirect_uri = self.form.oidc_redirect_uri.trim().to_string();
        c.capture.screenshot_interval_secs = self.form.screenshot_interval_secs;
        c.capture.diff_threshold_percent = self.form.diff_threshold_percent;
        c.file_tracker.watched_paths = self.form.watched_paths.clone();
        c.file_tracker.enabled = self.form.file_tracking_enabled;
        c.minio = MinioConfig {
            endpoint: self.form.minio_endpoint.clone(),
            bucket: self.form.minio_bucket.clone(),
            region: self.form.minio_region.clone(),
            access_key: self.form.minio_access_key.clone(),
            secret_key: self.form.minio_secret_key.clone(),
            use_ssl: self.form.minio_use_ssl,
        };
        c.postgres = PostgresConfig {
            host: self.form.postgres_host.clone(),
            port: self.form.postgres_port,
            user: self.form.postgres_user.clone(),
            password: self.form.postgres_password.clone(),
            database: self.form.postgres_database.clone(),
            require_ssl: self.form.postgres_require_ssl,
        };
        c.analysis.endpoint = self.form.analysis_endpoint.clone();
        c.analysis.api_key = self.form.analysis_api_key.clone();
        c.analysis.model = self.form.analysis_model.clone();
        c.ui_language = self.form.ui_language;
        if let Ok(uid) = Uuid::parse_str(self.form.user_id.trim()) {
            c.identity.user_id = Some(uid);
        }
        c
    }

    /// Spawn an async probe for `tab` and mark it running.
    fn run_test(&mut self, tab: SettingsTab, ctx: &egui::Context) {
        let config = self.build_config();
        self.tests.insert(tab, TestState::Running);
        let tx = self.test_tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = match tab {
                SettingsTab::Connection => connection_test::test_ci_server(&config).await,
                SettingsTab::Advanced => connection_test::test_vision(&config).await,
                #[cfg(feature = "legacy-minio")]
                SettingsTab::Minio => connection_test::test_minio(&config).await,
                #[cfg(feature = "legacy-postgres")]
                SettingsTab::Postgres => connection_test::test_postgres(&config).await,
                SettingsTab::Capture => return,
            };
            let _ = tx.send((tab, result));
            ctx.request_repaint();
        });
    }

    fn drain_discoveries(&mut self) {
        while let Ok(result) = self.discover_rx.try_recv() {
            self.discovering = false;
            match result {
                Ok(apps) => {
                    let candidates = crate::portal_apps::memory_candidates(&apps);
                    if candidates.is_empty() {
                        self.memory_status =
                            Some(rust_i18n::t!("settings.connection.memory_pick.empty").to_string());
                    } else {
                        self.memory_status = None;
                    }
                    self.memory_choices = candidates;
                }
                Err(err) => {
                    self.memory_status = Some(
                        rust_i18n::t!("settings.connection.memory_pick.error", error = err)
                            .to_string(),
                    );
                }
            }
        }
    }

    fn refresh_memories(&mut self, ctx: &egui::Context) {
        let (portal, token) = {
            let cfg = self.base_config.lock();
            let portal = cfg
                .oidc
                .issuer
                .trim()
                .trim_end_matches("/api/auth")
                .to_string();
            let token = cfg
                .oidc_session
                .as_ref()
                .and_then(|s| s.access_token.clone())
                .filter(|t| !t.trim().is_empty());
            (portal, token)
        };
        let Some(token) = token else {
            self.memory_status = Some(
                rust_i18n::t!("settings.connection.memory_pick.need_signin").to_string(),
            );
            return;
        };
        self.discovering = true;
        self.memory_status =
            Some(rust_i18n::t!("settings.connection.memory_pick.refreshing").to_string());
        let tx = self.discover_tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = match crate::portal_apps::fetch_portal_apps(
                &portal,
                &token,
                crate::portal_apps::MEMORY_APP_SLUG,
            )
            .await
            {
                crate::portal_apps::Discovery::Ok(apps) => Ok(apps),
                crate::portal_apps::Discovery::Unsupported => {
                    Err("Portal has no app discovery".into())
                }
                crate::portal_apps::Discovery::Unauthorized => {
                    Err("Portal refused the session".into())
                }
                crate::portal_apps::Discovery::Error(e) => Err(e),
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// Apply any finished test results.
    fn drain_tests(&mut self) {
        while let Ok((tab, result)) = self.test_rx.try_recv() {
            let state = match result {
                Ok(detail) => TestState::Ok(detail),
                Err(msg) => TestState::Err(msg),
            };
            self.tests.insert(tab, state);
        }
    }

    /// Derive a coarse connection descriptor from config presence (no network
    /// call is made here — this is a visual hint, not a live probe).
    fn connection_status(&self) -> (String, String, egui::Color32) {
        let has_url = !self.form.portal_url.trim().is_empty();
        let base = self.base_config.lock();
        let has_token = base
            .server
            .applet_auth_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
            || (self.form.auth_mode == AuthMode::ApiKey
                && !self.form.applet_auth_token.trim().is_empty());
        let signed_in_oidc = base.oidc_session.is_some();
        let device_pending = signed_in_oidc && base.identity.device_id.is_none() && has_url;
        drop(base);

        let (title, detail, color) = if has_url && has_token && device_pending {
            (
                rust_i18n::t!("settings.status.device_pending.title"),
                rust_i18n::t!("settings.status.device_pending.detail"),
                theme::AMBER(),
            )
        } else if has_url && has_token {
            (
                rust_i18n::t!("settings.status.connected.title"),
                rust_i18n::t!("settings.status.connected.detail"),
                theme::GREEN(),
            )
        } else if signed_in_oidc && !has_url {
            (
                rust_i18n::t!("settings.status.choose_memory.title"),
                rust_i18n::t!("settings.status.choose_memory.detail"),
                theme::AMBER(),
            )
        } else if has_url {
            (
                rust_i18n::t!("settings.status.almost.title"),
                rust_i18n::t!("settings.status.almost.detail"),
                theme::AMBER(),
            )
        } else {
            (
                rust_i18n::t!("settings.status.unset.title"),
                rust_i18n::t!("settings.status.unset.detail"),
                theme::RED(),
            )
        };
        (title.to_string(), detail.to_string(), color)
    }

    fn save(&mut self, ctx: &egui::Context) {
        if let Err(message) = self.validate() {
            self.error = Some(message);
            return;
        }

        let previous = self.base_config.lock().clone();
        let previous_user_id = previous.identity.user_id.map(|id| id.to_string());
        let previous_auth_token = previous.server.applet_auth_token.clone();

        let mut updated = self.build_config();
        // Identity is handled specially: a blank user clears the identity, and
        // changing the user resets the auto-assigned device.
        if self.form.user_id.trim().is_empty() {
            updated.identity.user_id = None;
            updated.identity.device_id = None;
        } else if previous_user_id.as_deref() != Some(self.form.user_id.trim()) {
            updated.identity.device_id = None;
        }
        // A *user-initiated* credential change (API-key mode) invalidates any
        // stored OIDC session. In OIDC mode the token is the id_token managed
        // by the login/refresh flow — build_config re-read it (and the
        // session) from disk, so a background rotation that landed while this
        // window was open must NOT be mistaken for a credential change and
        // wipe the session (which would permanently disarm the refresh loop).
        if self.form.auth_mode == AuthMode::ApiKey
            && previous_auth_token != updated.server.applet_auth_token
        {
            updated.oidc_session = None;
        }

        if let Err(err) = self.manager.save(&updated) {
            self.error = Some(
                rust_i18n::t!("settings.error.save_failed", error = err.to_string()).to_string(),
            );
            return;
        }

        *self.base_config.lock() = updated;
        *self.saved_flag.lock() = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn validate(&self) -> Result<(), String> {
        if !self.form.user_id.trim().is_empty() {
            Uuid::parse_str(self.form.user_id.trim())
                .map_err(|_| rust_i18n::t!("settings.error.user_id_uuid").to_string())?;
        }
        let has_server_credential = self.form.auth_mode == AuthMode::ApiKey
            && !self.form.applet_auth_token.trim().is_empty();
        let has_oidc_issuer = self.form.auth_mode == AuthMode::Oidc;

        if self.form.portal_url.trim().is_empty() && (has_server_credential || has_oidc_issuer) {
            return Err(rust_i18n::t!("settings.error.server_required").to_string());
        }
        if !self.form.portal_url.trim().is_empty() {
            normalize_server_url(self.form.portal_url.trim())?;
        }
        if self.form.auth_mode == AuthMode::ApiKey
            && !self.form.portal_url.trim().is_empty()
            && self.form.applet_auth_token.trim().is_empty()
        {
            return Err(rust_i18n::t!("settings.error.api_key_required").to_string());
        }
        if has_oidc_issuer {
            if self.form.oidc_issuer.trim().is_empty() {
                return Err(rust_i18n::t!("settings.error.oidc_issuer_required").to_string());
            }
            Url::parse(self.form.oidc_issuer.trim())
                .map_err(|_| rust_i18n::t!("settings.error.oidc_issuer_url").to_string())?;
            if self.form.oidc_client_id.trim().is_empty() {
                return Err(rust_i18n::t!("settings.error.oidc_client_id_required").to_string());
            }
            if self.form.oidc_scopes.trim().is_empty() {
                return Err(rust_i18n::t!("settings.error.oidc_scopes_required").to_string());
            }
            let redirect = Url::parse(self.form.oidc_redirect_uri.trim())
                .map_err(|_| rust_i18n::t!("settings.error.oidc_redirect_url").to_string())?;
            if redirect.scheme() != "http"
                || redirect.host_str() != Some("127.0.0.1")
                || redirect.port().is_none()
            {
                return Err(rust_i18n::t!("settings.error.oidc_redirect_loopback").to_string());
            }
        }
        if self.form.screenshot_interval_secs == 0 {
            return Err(rust_i18n::t!("settings.error.interval_zero").to_string());
        }
        if !(0.0..=100.0).contains(&self.form.diff_threshold_percent) {
            return Err(rust_i18n::t!("settings.error.diff_range").to_string());
        }
        // Deliberately no Postgres port check. The default build never opens a
        // database connection, so a stale `postgres.port: 0` in an old
        // config.json must not block saving a perfectly valid server URL with an
        // error about infrastructure this machine doesn't use.
        Ok(())
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::sync(ctx);
        theme::center_once(ctx, &mut self.centered);
        self.drain_tests();
        self.drain_discoveries();
        self.render_sidebar(ctx);
        self.render_footer(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL())
                    .inner_margin(egui::Margin::symmetric(30, 26)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let tab = self.current_tab;
                        let changed = match tab {
                            SettingsTab::Connection => render_connection_tab(ui, self, ctx),
                            SettingsTab::Capture => render_capture_tab(ui, &mut self.form),
                            SettingsTab::Advanced => render_advanced_tab(ui, &mut self.form),
                            #[cfg(feature = "legacy-minio")]
                            SettingsTab::Minio => render_minio_tab(ui, &mut self.form),
                            #[cfg(feature = "legacy-postgres")]
                            SettingsTab::Postgres => render_postgres_tab(ui, &mut self.form),
                        };
                        // Editing a field invalidates a prior test result.
                        if changed {
                            self.error = None;
                            self.tests.remove(&tab);
                        }
                        if tab.has_connection() {
                            ui.add_space(20.0);
                            ui.separator();
                            ui.add_space(14.0);
                            if render_test_row(ui, self.tests.get(&tab)) {
                                self.run_test(tab, ctx);
                            }
                        }
                    });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !*self.saved_flag.lock() {
            *self.saved_flag.lock() = false;
        }
    }
}

impl SettingsApp {
    fn render_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(236.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR())
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                // Header: logo + product name.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    theme::logo(ui, &self.logo, 26.0, theme::TEAL());
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            egui::RichText::new("Companion Capture")
                                .font(theme::light(18.0))
                                .color(theme::TEXT()),
                        );
                        ui.label(
                            egui::RichText::new(
                                rust_i18n::t!("settings.sidebar.subtitle").to_string(),
                            )
                            .font(theme::body(13.0))
                            .color(theme::MUTED()),
                        );
                    });
                });
                ui.add_space(14.0);

                // Every tab is always shown. The legacy Storage/Database tabs
                // are compiled out of a default build rather than hidden, so
                // there is nothing left to reveal at runtime.
                for tab in SettingsTab::all() {
                    let selected = self.current_tab == tab;
                    if theme::nav_item(ui, tab.accent(), &tab.label(), &tab.subtitle(), selected) {
                        self.current_tab = tab;
                    }
                    ui.add_space(3.0);
                }

                // Push the connection card to the bottom.
                let (text, sub, color) = self.connection_status();
                let lanes = rust_i18n::t!("settings.sidebar.lanes").to_string();
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    theme::card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            theme::status_dot(ui, color, true);
                            ui.label(
                                egui::RichText::new(text)
                                    .font(theme::semibold(14.0))
                                    .color(theme::TEXT()),
                            );
                        });
                        ui.add_space(5.0);
                        theme::caption(ui, &sub);
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for c in [
                                theme::LANE_SHOT(),
                                theme::LANE_ACTIVITY(),
                                theme::LANE_FILE(),
                            ] {
                                let (r, _) = ui.allocate_exact_size(
                                    egui::Vec2::splat(7.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().circle_filled(r.center(), 3.5, c);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| theme::mono_label(ui, &lanes, theme::FAINT()),
                            );
                        });
                    });
                });
            });
    }

    fn render_footer(&mut self, ctx: &egui::Context) {
        let config_path = self.manager.path().display().to_string();
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_HI())
                    .inner_margin(egui::Margin::symmetric(22, 14)),
            )
            .show(ctx, |ui| {
                // Right-to-left for the whole row: the buttons reserve their
                // width first and the status text truncates into the remainder.
                // Laying the text out first instead let a long config path run
                // underneath Close/Save.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if theme::primary_button(ui, &rust_i18n::t!("settings.footer.save")).clicked() {
                        self.save(ctx);
                    }
                    if theme::ghost_button(ui, &rust_i18n::t!("settings.footer.close")).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if let Some(error) = self.error.clone() {
                            theme::status_dot(ui, theme::RED(), false);
                            ui.label(
                                egui::RichText::new(error)
                                    .font(theme::body(tokens::text::SMALL))
                                    .color(theme::RED()),
                            );
                        } else {
                            // `truncate` is load-bearing: an egui label does not
                            // clip to its rect by default, so a long config path
                            // renders straight over the buttons to its right.
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&config_path)
                                        .font(theme::mono(tokens::text::MIN))
                                        .color(theme::FAINT()),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&config_path);
                        }
                    });
                });
            });
    }
}

/// A labelled text field with consistent spacing. Returns whether it changed.
fn field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    opts: theme::FieldOpts,
) -> bool {
    theme::field_label(ui, label);
    ui.add_space(6.0);
    let changed = theme::text_field(ui, value, hint, opts);
    ui.add_space(15.0);
    changed
}

/// The "Test connection" button + live result for a connection tab. Returns
/// true when the button is clicked.
fn render_test_row(ui: &mut egui::Ui, state: Option<&TestState>) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        clicked = theme::ghost_button(ui, &rust_i18n::t!("settings.test.button")).clicked();
        match state {
            Some(TestState::Running) => {
                ui.add(egui::Spinner::new().size(16.0).color(theme::AMBER()));
                ui.label(
                    egui::RichText::new(rust_i18n::t!("settings.test.running").to_string())
                        .font(theme::body(13.0))
                        .color(theme::AMBER()),
                );
            }
            Some(TestState::Ok(_)) => {
                theme::status_badge(ui, &rust_i18n::t!("settings.test.ok"), theme::GREEN());
            }
            Some(TestState::Err(_)) => {
                theme::status_badge(ui, &rust_i18n::t!("settings.test.failed"), theme::RED());
            }
            None => {}
        }
    });
    if let Some(TestState::Ok(detail) | TestState::Err(detail)) = state {
        ui.add_space(7.0);
        theme::mono_label(ui, detail, theme::MUTED());
    }
    clicked
}

/// Page title + descriptive subtitle.
fn page_header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    theme::heading(ui, title);
    ui.add_space(3.0);
    theme::subtitle(ui, subtitle);
    ui.add_space(20.0);
}

fn render_connection_tab(
    ui: &mut egui::Ui,
    app: &mut SettingsApp,
    ctx: &egui::Context,
) -> bool {
    let mut changed = false;
    page_header(
        ui,
        &SettingsTab::Connection.label(),
        &rust_i18n::t!("settings.connection.subtitle"),
    );

    // Memory picker — Portal discovery by Hub device (many Captures may share one).
    theme::field_label(ui, &rust_i18n::t!("settings.connection.memory_pick.label"));
    ui.add_space(4.0);
    theme::caption(ui, &rust_i18n::t!("settings.connection.memory_pick.help"));
    ui.add_space(8.0);
    if !app.form.bound_device_name.trim().is_empty() {
        ui.label(
            egui::RichText::new(
                rust_i18n::t!(
                    "settings.connection.memory_pick.bound",
                    device = app.form.bound_device_name.as_str()
                )
                .to_string(),
            )
            .color(theme::TEAL()),
        );
        ui.add_space(6.0);
    }
    let refresh_label = if app.discovering {
        rust_i18n::t!("settings.connection.memory_pick.refreshing").to_string()
    } else {
        rust_i18n::t!("settings.connection.memory_pick.refresh").to_string()
    };
    if ui
        .add_enabled(!app.discovering, egui::Button::new(refresh_label))
        .clicked()
    {
        app.refresh_memories(ctx);
    }
    if let Some(status) = &app.memory_status {
        ui.add_space(4.0);
        theme::caption(ui, status);
    }
    let choices = app.memory_choices.clone();
    for choice in &choices {
        let label = choice
            .device_name
            .clone()
            .or_else(|| choice.device_slug.clone())
            .unwrap_or_else(|| choice.url.clone());
        let selected = app.form.bound_device_slug.as_str()
            == choice.device_slug.as_deref().unwrap_or("")
            || (!app.form.portal_url.is_empty() && app.form.portal_url == choice.url);
        if ui.selectable_label(selected, &label).clicked() {
            app.form.portal_url = choice.url.clone();
            app.form.bound_device_slug = choice.device_slug.clone().unwrap_or_default();
            app.form.bound_device_name = choice.device_name.clone().unwrap_or_default();
            app.form.memory_from_discovery = true;
            changed = true;
        }
    }
    ui.add_space(16.0);

    let url_before = app.form.portal_url.clone();
    let form = &mut app.form;
    let url_edited = field(
        ui,
        &rust_i18n::t!("settings.connection.server.label"),
        &mut form.portal_url,
        &rust_i18n::t!("settings.connection.server.hint"),
        theme::FieldOpts::monospace(),
    );
    // Typing over a discovered URL → manual provenance on save.
    if url_edited && form.portal_url != url_before {
        form.memory_from_discovery = false;
        form.bound_device_slug.clear();
        form.bound_device_name.clear();
    }
    changed |= url_edited;
    theme::caption(ui, &rust_i18n::t!("settings.connection.server.help"));
    ui.add_space(6.0);

    // A plain-http origin that isn't loopback means captures cross the network
    // in the clear. Not a blocker — a LAN server has no cert — but never silent.
    let target = form.portal_url.trim();
    if target.starts_with("http://")
        && !target.contains("//localhost")
        && !target.contains("//127.0.0.1")
    {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(
                    rust_i18n::t!("settings.connection.insecure_warning").to_string(),
                )
                .font(theme::body(tokens::text::SMALL))
                .color(theme::AMBER()),
            );
        });
        ui.add_space(10.0);
    }

    theme::field_label(ui, &rust_i18n::t!("settings.connection.auth.label"));
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::INPUT())
        .stroke(egui::Stroke::new(1.0, theme::BORDER()))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if theme::segment(
                    ui,
                    &rust_i18n::t!("settings.connection.auth.api_key"),
                    form.auth_mode == AuthMode::ApiKey,
                ) {
                    form.auth_mode = AuthMode::ApiKey;
                    changed = true;
                }
                if theme::segment(
                    ui,
                    &rust_i18n::t!("settings.connection.auth.oidc"),
                    form.auth_mode == AuthMode::Oidc,
                ) {
                    form.auth_mode = AuthMode::Oidc;
                    changed = true;
                }
            });
        });
    ui.add_space(16.0);

    match form.auth_mode {
        AuthMode::ApiKey => {
            changed |= field(
                ui,
                &rust_i18n::t!("settings.connection.api_key.label"),
                &mut form.applet_auth_token,
                &rust_i18n::t!("settings.connection.api_key.hint"),
                theme::FieldOpts::password(),
            );
            theme::caption(ui, &rust_i18n::t!("settings.connection.api_key.help"));
            ui.add_space(15.0);
        }
        AuthMode::Oidc => {
            theme::caption(ui, &rust_i18n::t!("settings.connection.oidc.help"));
            ui.add_space(10.0);

            // OIDC wiring. Correct out of the box for the production Portal, so
            // this is collapsed: a working install should never need to open it,
            // and four monospace URLs on the main pane read as "you must
            // configure this" when you must not.
            egui::CollapsingHeader::new(
                egui::RichText::new(rust_i18n::t!("settings.connection.oidc.details").to_string())
                    .font(theme::body(tokens::text::SMALL))
                    .color(theme::MUTED()),
            )
            .id_salt("settings_oidc_details")
            .show(ui, |ui| {
                ui.add_space(6.0);
                // The hints below are literal protocol values (a URL, the
                // frozen `ci-applet` client id, OIDC scope tokens) — not prose,
                // and not translatable.
                changed |= field(
                    ui,
                    &rust_i18n::t!("settings.connection.oidc.issuer.label"),
                    &mut form.oidc_issuer,
                    "https://hub.ci.computer/api/auth",
                    theme::FieldOpts::monospace(),
                );
                ui.columns(2, |cols| {
                    changed |= field(
                        &mut cols[0],
                        &rust_i18n::t!("settings.connection.oidc.client_id.label"),
                        &mut form.oidc_client_id,
                        "ci-applet",
                        theme::FieldOpts::monospace(),
                    );
                    changed |= field(
                        &mut cols[1],
                        &rust_i18n::t!("settings.connection.oidc.scopes.label"),
                        &mut form.oidc_scopes,
                        "openid profile email",
                        theme::FieldOpts::monospace(),
                    );
                });
                changed |= field(
                    ui,
                    &rust_i18n::t!("settings.connection.oidc.redirect.label"),
                    &mut form.oidc_redirect_uri,
                    "http://127.0.0.1:0/callback",
                    theme::FieldOpts::monospace(),
                );
            });
        }
    }

    ui.add_space(14.0);
    theme::field_label(ui, &rust_i18n::t!("settings.connection.device.label"));
    ui.add_space(8.0);
    theme::card(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.columns(2, |cols| {
            theme::caption(
                &mut cols[0],
                &rust_i18n::t!("settings.connection.device.user"),
            );
            cols[0].add_space(4.0);
            if form.user_id.is_empty() {
                theme::mono_label(
                    &mut cols[0],
                    &rust_i18n::t!("settings.connection.device.user_placeholder"),
                    theme::FAINT(),
                );
            } else {
                theme::mono_label(&mut cols[0], &form.user_id, theme::MUTED());
            }

            theme::caption(
                &mut cols[1],
                &rust_i18n::t!("settings.connection.device.device"),
            );
            cols[1].add_space(4.0);
            if form.device_id.is_empty() {
                theme::mono_label(
                    &mut cols[1],
                    &rust_i18n::t!("settings.connection.device.device_placeholder"),
                    theme::FAINT(),
                );
            } else {
                theme::mono_label(&mut cols[1], &form.device_id, theme::MUTED());
            }
        });
    });
    changed
}

/// Settings that exist for people with an unusual setup. Everything here has a
/// working default and an empty state that means "let my Companion Memory server handle it".
fn render_advanced_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        &SettingsTab::Advanced.label(),
        &rust_i18n::t!("settings.advanced.subtitle"),
    );

    theme::field_label(ui, &rust_i18n::t!("settings.advanced.vision.label"));
    ui.add_space(8.0);

    let local = !form.analysis_endpoint.trim().is_empty();
    theme::card(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            theme::status_dot(
                ui,
                if local {
                    theme::AMBER()
                } else {
                    theme::GREEN()
                },
                false,
            );
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.label(
                    egui::RichText::new(
                        if local {
                            rust_i18n::t!("settings.advanced.vision.local_title")
                        } else {
                            rust_i18n::t!("settings.advanced.vision.remote_title")
                        }
                        .to_string(),
                    )
                    .font(theme::semibold(tokens::text::BODY))
                    .color(theme::TEXT()),
                );
                theme::caption(
                    ui,
                    &if local {
                        rust_i18n::t!("settings.advanced.vision.local_help")
                    } else {
                        rust_i18n::t!("settings.advanced.vision.remote_help")
                    },
                );
            });
        });
    });
    ui.add_space(12.0);

    // The endpoint is an unrestricted URL — it will happily accept a public
    // cloud API. Off by default is the right default, but a user who fills it
    // in deserves to know exactly what it does before they do.
    if local {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(rust_i18n::t!("settings.advanced.vision.warning").to_string())
                    .font(theme::body(tokens::text::SMALL))
                    .color(theme::AMBER()),
            );
        });
        ui.add_space(10.0);
    }

    egui::CollapsingHeader::new(
        egui::RichText::new(rust_i18n::t!("settings.advanced.vision.toggle").to_string())
            .font(theme::body(tokens::text::SMALL))
            .color(theme::MUTED()),
    )
    .id_salt("settings_local_vision")
    .default_open(local)
    .show(ui, |ui| {
        ui.add_space(6.0);
        // The client POSTs `{endpoint}/chat/completions` with Bearer auth (OpenAI
        // protocol), so this is a base URL — NOT an Anthropic /messages URL.
        changed |= field(
            ui,
            &rust_i18n::t!("settings.advanced.endpoint.label"),
            &mut form.analysis_endpoint,
            &rust_i18n::t!("settings.advanced.endpoint.hint"),
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            ui,
            &rust_i18n::t!("settings.advanced.api_key.label"),
            &mut form.analysis_api_key,
            &rust_i18n::t!("settings.advanced.api_key.hint"),
            theme::FieldOpts::password(),
        );
        changed |= field(
            ui,
            &rust_i18n::t!("settings.advanced.model.label"),
            &mut form.analysis_model,
            // A model identifier, not prose.
            "lmstudio-community/vision-large",
            theme::FieldOpts::monospace(),
        );
    });

    ui.add_space(20.0);
    // Language belongs on this page precisely because the default —
    // `Language::System` — already does the right thing for almost everyone.
    // You only come looking for this control when you want an app language
    // that differs from your OS, which is exactly what "unusual setups" means.
    //
    // Picking a language repaints *this* window immediately (see the `apply`
    // below). A language picker whose own window stays in the old language
    // gives the user no way to tell whether the click registered — and if they
    // picked wrong, no way to read the label that would let them undo it.
    // Saving is what makes the choice stick and relabels the tray.
    theme::field_label(ui, &rust_i18n::t!("settings.language.label"));
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::INPUT())
        .stroke(egui::Stroke::new(1.0, theme::BORDER()))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for language in crate::locale::Language::ALL {
                    // `label()` is written in the language it names, so it is
                    // deliberately not translated per-locale.
                    if theme::segment(ui, &language.label(), form.ui_language == language) {
                        form.ui_language = language;
                        // Repaint this window in the chosen language right away.
                        // Safe to do before Save: the locale is per-process
                        // state, so abandoning the dialog without saving leaves
                        // nothing behind — the next launch reads config again.
                        crate::locale::apply(language);
                        changed = true;
                    }
                }
            });
        });
    ui.add_space(8.0);
    theme::caption(ui, &rust_i18n::t!("settings.language.help"));

    changed
}

#[cfg(test)]
mod tests {
    use super::{AuthMode, SettingsFormState};
    use crate::config::{AppConfig, OidcSession};
    use crate::server_common::normalize_server_url;

    #[test]
    fn oidc_session_infers_oidc_mode_not_api_key() {
        // Post-login state: the id_token lives in applet_auth_token AND an
        // oidc_session is present. Must read as OIDC, not API key (which would
        // render the raw JWT in the key field).
        let mut config = AppConfig::default();
        config.server.applet_auth_token = Some("eyJhbGciOi.header.sig".to_string());
        config.oidc_session = Some(OidcSession {
            id_token: Some("eyJhbGciOi.header.sig".to_string()),
            ..OidcSession::default()
        });
        assert_eq!(SettingsFormState::from(&config).auth_mode, AuthMode::Oidc);
    }

    #[test]
    fn pasted_key_without_session_infers_api_key_mode() {
        let mut config = AppConfig::default();
        config.server.applet_auth_token = Some("deadbeef".repeat(8)); // 64-hex-ish key
        assert_eq!(SettingsFormState::from(&config).auth_mode, AuthMode::ApiKey);
    }

    #[test]
    fn issuer_only_infers_oidc_mode() {
        let mut config = AppConfig::default();
        config.oidc.issuer = "https://hub.example.com/api/auth".to_string();
        assert_eq!(SettingsFormState::from(&config).auth_mode, AuthMode::Oidc);
    }

    #[test]
    fn feed_page_url_normalizes_to_origin() {
        assert_eq!(
            normalize_server_url("https://ci-server.localhost/feed").unwrap(),
            "https://ci-server.localhost/"
        );
        assert_eq!(
            normalize_server_url("https://ci.example.com/feed?cursor=abc").unwrap(),
            "https://ci.example.com/"
        );
    }

    #[test]
    fn api_base_path_is_preserved() {
        assert_eq!(
            normalize_server_url("https://ci.example.com/api").unwrap(),
            "https://ci.example.com/api"
        );
    }
}

fn render_capture_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        &SettingsTab::Capture.label(),
        &rust_i18n::t!("settings.capture.subtitle"),
    );

    ui.columns(2, |cols| {
        // Screenshot interval stepper.
        theme::card(&mut cols[0], |ui| {
            ui.set_width(ui.available_width());
            theme::field_label(ui, &rust_i18n::t!("settings.capture.interval.label"));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                if stepper_button(ui, "\u{2212}") {
                    form.screenshot_interval_secs =
                        form.screenshot_interval_secs.saturating_sub(30).max(30);
                    changed = true;
                }
                ui.with_layout(
                    egui::Layout::top_down(egui::Align::Center).with_main_justify(true),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(form.screenshot_interval_secs.to_string())
                                    .font(theme::light(34.0))
                                    .color(theme::TEXT()),
                            );
                            ui.label(
                                egui::RichText::new("s")
                                    .font(theme::body(16.0))
                                    .color(theme::MUTED()),
                            );
                        });
                    },
                );
                if stepper_button(ui, "+") {
                    form.screenshot_interval_secs =
                        (form.screenshot_interval_secs + 30).min(86_400);
                    changed = true;
                }
            });
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                theme::caption(ui, &rust_i18n::t!("settings.capture.interval.help"))
            });
        });

        // Diff threshold slider.
        theme::card(&mut cols[1], |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::field_label(ui, &rust_i18n::t!("settings.capture.diff.label"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("%")
                            .font(theme::body(15.0))
                            .color(theme::TEAL()),
                    );
                    ui.label(
                        egui::RichText::new(format!("{:.0}", form.diff_threshold_percent))
                            .font(theme::light(26.0))
                            .color(theme::TEAL()),
                    );
                });
            });
            ui.add_space(12.0);
            changed |= ui
                .add(
                    egui::Slider::new(&mut form.diff_threshold_percent, 0.0..=100.0)
                        .show_value(false),
                )
                .changed();
            ui.add_space(12.0);
            theme::caption(ui, &rust_i18n::t!("settings.capture.diff.help"));
        });
    });

    ui.add_space(20.0);
    theme::field_label(ui, &rust_i18n::t!("settings.capture.lanes.label"));
    ui.add_space(10.0);
    // Plain language: these rows describe what the user gets, not how it is
    // implemented. "vizier", "JSON-Patch" and "durable queue" are internals and
    // told a first-time reader nothing.
    let on = rust_i18n::t!("settings.capture.lane.on").to_string();
    theme::lane_row(
        ui,
        theme::LANE_SHOT(),
        &rust_i18n::t!("settings.capture.lane.screens.title"),
        &rust_i18n::t!("settings.capture.lane.screens.detail"),
        &on,
    );
    ui.add_space(10.0);
    theme::lane_row(
        ui,
        theme::LANE_ACTIVITY(),
        &rust_i18n::t!("settings.capture.lane.activity.title"),
        &rust_i18n::t!("settings.capture.lane.activity.detail"),
        &on,
    );
    ui.add_space(10.0);
    theme::lane_row(
        ui,
        theme::LANE_FILE(),
        &rust_i18n::t!("settings.capture.lane.files.title"),
        &rust_i18n::t!("settings.capture.lane.files.detail"),
        &if form.watched_paths.is_empty() {
            rust_i18n::t!("settings.capture.lane.off")
        } else {
            rust_i18n::t!("settings.capture.lane.on")
        },
    );

    changed |= render_watched_folders(ui, form);
    changed
}

/// The watched-folder list — the control that arms the file-tracking lane.
///
/// Until this existed the onboarding wizard told users to "open Settings to add
/// folders you want tracked" against a Settings window that had no such control,
/// leaving the whole watcher/queue/extractor path reachable only by hand-editing
/// `config.json`.
fn render_watched_folders(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;

    ui.add_space(20.0);
    ui.horizontal(|ui| {
        theme::field_label(ui, &rust_i18n::t!("settings.folders.label"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary_button(ui, &rust_i18n::t!("settings.folders.add")).clicked() {
                // Start the picker somewhere useful rather than at "/".
                let start = form
                    .watched_paths
                    .last()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                    .or_else(|| directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()))
                    .unwrap_or_default();
                if let Some(picked) = tinyfiledialogs::select_folder_dialog(
                    &rust_i18n::t!("settings.folders.picker_title"),
                    &start.to_string_lossy(),
                ) {
                    let path = PathBuf::from(picked);
                    if form.watched_paths.contains(&path) {
                        // Silently ignoring a duplicate looks like the click
                        // did nothing; adding it twice would double-watch.
                    } else if let Some(parent) = form
                        .watched_paths
                        .iter()
                        .find(|existing| path.starts_with(existing))
                    {
                        log::info!(
                            "not adding {} — already covered by watched folder {}",
                            path.display(),
                            parent.display()
                        );
                    } else {
                        // Adding a parent supersedes any nested folders already
                        // listed; keeping both would emit duplicate events.
                        form.watched_paths.retain(|p| !p.starts_with(&path));
                        form.watched_paths.push(path);
                        form.file_tracking_enabled = true;
                        changed = true;
                    }
                }
            }
        });
    });
    ui.add_space(10.0);

    if form.watched_paths.is_empty() {
        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subtitle(ui, &rust_i18n::t!("settings.folders.empty"));
        });
    } else {
        let mut remove = None;
        for (i, path) in form.watched_paths.iter().enumerate() {
            theme::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let exists = path.is_dir();
                    theme::status_dot(
                        ui,
                        if exists {
                            theme::GREEN()
                        } else {
                            theme::AMBER()
                        },
                        false,
                    );
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            egui::RichText::new(
                                path.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.to_string_lossy().to_string()),
                            )
                            .font(theme::body(tokens::text::BODY))
                            .color(theme::TEXT()),
                        );
                        theme::mono_label(
                            ui,
                            &path.to_string_lossy(),
                            if exists {
                                theme::MUTED()
                            } else {
                                theme::AMBER()
                            },
                        );
                        if !exists {
                            theme::caption(ui, &rust_i18n::t!("settings.folders.missing"));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::ghost_button(ui, &rust_i18n::t!("settings.folders.remove"))
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
        if let Some(i) = remove {
            form.watched_paths.remove(i);
            changed = true;
        }

        ui.add_space(4.0);
        changed |= theme::toggle_row(
            ui,
            &mut form.file_tracking_enabled,
            &rust_i18n::t!("settings.folders.toggle.title"),
            &rust_i18n::t!("settings.folders.toggle.detail"),
        );
    }

    changed
}

/// A small square +/- button for the interval stepper.
fn stepper_button(ui: &mut egui::Ui, glyph: &str) -> bool {
    let btn = egui::Button::new(
        egui::RichText::new(glyph)
            .font(theme::body(20.0))
            .color(theme::TEXT()),
    )
    .fill(theme::INPUT())
    .stroke(egui::Stroke::new(1.0, theme::BORDER_HI()))
    .corner_radius(egui::CornerRadius::same(9))
    .min_size(egui::Vec2::splat(34.0));
    ui.add(btn).clicked()
}

/// Legacy: writes screenshot bytes straight to a MinIO/S3 bucket, bypassing the
/// Companion Memory server. Compiled out of a default build.
#[cfg(feature = "legacy-minio")]
fn render_minio_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        &SettingsTab::Minio.label(),
        &rust_i18n::t!("settings.storage.subtitle"),
    );

    changed |= field(
        ui,
        &rust_i18n::t!("settings.storage.endpoint.label"),
        &mut form.minio_endpoint,
        "http://localhost:9000",
        theme::FieldOpts::monospace(),
    );
    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            &rust_i18n::t!("settings.storage.bucket.label"),
            &mut form.minio_bucket,
            "ci-captures",
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            &rust_i18n::t!("settings.storage.region.label"),
            &mut form.minio_region,
            "us-east-1",
            theme::FieldOpts::monospace(),
        );
    });
    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            &rust_i18n::t!("settings.storage.access_key.label"),
            &mut form.minio_access_key,
            &rust_i18n::t!("settings.storage.access_key.hint"),
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            &rust_i18n::t!("settings.storage.secret_key.label"),
            &mut form.minio_secret_key,
            &rust_i18n::t!("settings.storage.secret_key.hint"),
            theme::FieldOpts::password(),
        );
    });
    changed |= theme::toggle_row(
        ui,
        &mut form.minio_use_ssl,
        &rust_i18n::t!("settings.storage.ssl.title"),
        &rust_i18n::t!("settings.storage.ssl.detail"),
    );
    changed
}

/// Legacy: opens a plaintext database connection from this machine. Compiled
/// out of a default build — see [`crate::db`].
#[cfg(feature = "legacy-postgres")]
fn render_postgres_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        &SettingsTab::Postgres.label(),
        &rust_i18n::t!("settings.database.subtitle"),
    );

    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            &rust_i18n::t!("settings.database.host.label"),
            &mut form.postgres_host,
            "db.example.com",
            theme::FieldOpts::monospace(),
        );
        theme::field_label(&mut cols[1], &rust_i18n::t!("settings.database.port.label"));
        cols[1].add_space(6.0);
        changed |= cols[1]
            .add(
                egui::DragValue::new(&mut form.postgres_port)
                    .range(1..=65535)
                    .speed(1.0),
            )
            .changed();
        cols[1].add_space(15.0);
    });
    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            &rust_i18n::t!("settings.database.name.label"),
            &mut form.postgres_database,
            "companion",
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            &rust_i18n::t!("settings.database.user.label"),
            &mut form.postgres_user,
            "ci_applet",
            theme::FieldOpts::monospace(),
        );
    });
    changed |= field(
        ui,
        &rust_i18n::t!("settings.database.password.label"),
        &mut form.postgres_password,
        &rust_i18n::t!("settings.database.password.hint"),
        theme::FieldOpts::password(),
    );
    changed |= theme::toggle_row(
        ui,
        &mut form.postgres_require_ssl,
        &rust_i18n::t!("settings.database.ssl.title"),
        &rust_i18n::t!("settings.database.ssl.detail"),
    );
    changed
}
