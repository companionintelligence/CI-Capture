use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context, Result, anyhow};
use eframe::{NativeOptions, egui};
use parking_lot::Mutex;
use reqwest::Url;
use uuid::Uuid;

use crate::config::{AppConfig, ConfigManager, MinioConfig, PostgresConfig};
use crate::connection_test;
use crate::theme;

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

    let manager = Arc::new(manager);
    let shared_saved = Arc::new(Mutex::new(false));
    let base_config = Arc::new(Mutex::new(config.clone()));

    let manager_clone = manager.clone();
    let saved_clone = shared_saved.clone();
    let base_clone = base_config.clone();

    let mut native_options = NativeOptions::default();
    native_options.viewport = egui::ViewportBuilder::default()
        .with_title("CI-Applet Settings")
        .with_inner_size([900.0, 620.0])
        .with_min_inner_size([720.0, 520.0]);

    eframe::run_native(
        "CI-Applet Settings",
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
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_scopes: String,
    pub oidc_redirect_uri: String,
    pub applet_auth_token: String,
    pub screenshot_interval_secs: u64,
    pub diff_threshold_percent: f32,
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
            oidc_issuer: config.oidc.issuer.clone(),
            oidc_client_id: config.oidc.client_id.clone(),
            oidc_scopes: config.oidc.scopes.clone(),
            oidc_redirect_uri: config.oidc.redirect_uri.clone(),
            applet_auth_token: config.server.applet_auth_token.clone().unwrap_or_default(),
            screenshot_interval_secs: config.capture.screenshot_interval_secs,
            diff_threshold_percent: config.capture.diff_threshold_percent,
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
            auth_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Oidc,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsTab {
    Identity,
    Capture,
    Minio,
    Postgres,
    Analysis,
}

impl SettingsTab {
    fn label(&self) -> &'static str {
        match self {
            SettingsTab::Identity => "Identity",
            SettingsTab::Capture => "Capture",
            SettingsTab::Minio => "Storage",
            SettingsTab::Postgres => "Database",
            SettingsTab::Analysis => "Vision",
        }
    }

    /// Short descriptor shown under the nav label.
    fn subtitle(&self) -> &'static str {
        match self {
            SettingsTab::Identity => "Server & auth",
            SettingsTab::Capture => "Cadence & lanes",
            SettingsTab::Minio => "MinIO object store",
            SettingsTab::Postgres => "Postgres",
            SettingsTab::Analysis => "Analysis model",
        }
    }

    /// Accent colour for the nav chip.
    fn accent(&self) -> egui::Color32 {
        match self {
            SettingsTab::Identity => theme::TEAL_DIM,
            SettingsTab::Capture => theme::BLUE,
            SettingsTab::Minio => theme::PURPLE,
            SettingsTab::Postgres => theme::TEAL_DIM,
            SettingsTab::Analysis => theme::BLUE,
        }
    }

    /// Whether this tab configures a remote service that can be test-probed.
    fn has_connection(&self) -> bool {
        !matches!(self, SettingsTab::Capture)
    }

    /// Legacy direct-write connections (Storage/Database). The supported path
    /// streams everything through CI-Server, so these are hidden unless the
    /// user opts into the advanced section.
    fn is_advanced(&self) -> bool {
        matches!(self, SettingsTab::Minio | SettingsTab::Postgres)
    }

    fn all() -> [SettingsTab; 5] {
        [
            SettingsTab::Identity,
            SettingsTab::Capture,
            SettingsTab::Minio,
            SettingsTab::Postgres,
            SettingsTab::Analysis,
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
    show_advanced: bool,
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
        Self {
            manager,
            base_config,
            form,
            current_tab: SettingsTab::Identity,
            error: None,
            saved_flag,
            logo,
            runtime,
            test_tx,
            test_rx,
            tests: HashMap::new(),
            show_advanced: false,
        }
    }

    /// Build an `AppConfig` from the current form, layered onto the last saved
    /// config. Shared by `save` and the connection tests so a test always
    /// probes exactly what the form shows.
    fn build_config(&self) -> AppConfig {
        let mut c = self.base_config.lock().clone();
        c.server.portal_url = normalize_ci_server_url(self.form.portal_url.trim())
            .unwrap_or_else(|_| self.form.portal_url.trim().to_string());
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
                SettingsTab::Identity => connection_test::test_ci_server(&config).await,
                SettingsTab::Minio => connection_test::test_minio(&config).await,
                SettingsTab::Postgres => connection_test::test_postgres(&config).await,
                SettingsTab::Analysis => connection_test::test_vision(&config).await,
                SettingsTab::Capture => return,
            };
            let _ = tx.send((tab, result));
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
    fn connection_status(&self) -> (&'static str, &'static str, egui::Color32) {
        let has_url = !self.form.portal_url.trim().is_empty();
        let has_cred = match self.form.auth_mode {
            AuthMode::ApiKey => !self.form.applet_auth_token.trim().is_empty(),
            AuthMode::Oidc => !self.form.oidc_issuer.trim().is_empty(),
        };
        if has_url && has_cred {
            ("Configured", "Streaming target set", theme::GREEN)
        } else if has_url {
            ("Incomplete", "Add credentials to stream", theme::AMBER)
        } else {
            ("Not configured", "Set a CI-Server URL", theme::RED)
        }
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
            self.error = Some(format!("Failed to save config: {err}"));
            return;
        }

        *self.base_config.lock() = updated;
        *self.saved_flag.lock() = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn validate(&self) -> Result<(), String> {
        if !self.form.user_id.trim().is_empty() {
            Uuid::parse_str(self.form.user_id.trim())
                .map_err(|_| "User ID must be a valid UUID.".to_string())?;
        }
        let has_server_credential = self.form.auth_mode == AuthMode::ApiKey
            && !self.form.applet_auth_token.trim().is_empty();
        let has_oidc_issuer = self.form.auth_mode == AuthMode::Oidc;

        if self.form.portal_url.trim().is_empty() && (has_server_credential || has_oidc_issuer) {
            return Err("CI-Server URL is required.".into());
        }
        if !self.form.portal_url.trim().is_empty() {
            normalize_ci_server_url(self.form.portal_url.trim())?;
        }
        if self.form.auth_mode == AuthMode::ApiKey
            && !self.form.portal_url.trim().is_empty()
            && self.form.applet_auth_token.trim().is_empty()
        {
            return Err("API key is required in API key mode.".into());
        }
        if has_oidc_issuer {
            if self.form.oidc_issuer.trim().is_empty() {
                return Err("OIDC issuer is required in OIDC mode.".into());
            }
            Url::parse(self.form.oidc_issuer.trim())
                .map_err(|_| "OIDC issuer must be a valid URL.".to_string())?;
            if self.form.oidc_client_id.trim().is_empty() {
                return Err("OIDC client ID cannot be empty.".into());
            }
            if self.form.oidc_scopes.trim().is_empty() {
                return Err("OIDC scopes cannot be empty.".into());
            }
            let redirect = Url::parse(self.form.oidc_redirect_uri.trim())
                .map_err(|_| "OIDC redirect URI must be a valid URL.".to_string())?;
            if redirect.scheme() != "http"
                || redirect.host_str() != Some("127.0.0.1")
                || redirect.port().is_none()
            {
                return Err(
                    "OIDC redirect URI must be an http://127.0.0.1:<port>/... loopback URL."
                        .to_string(),
                );
            }
        }
        if self.form.screenshot_interval_secs == 0 {
            return Err("Screenshot interval must be greater than zero.".into());
        }
        if !(0.0..=100.0).contains(&self.form.diff_threshold_percent) {
            return Err("Diff threshold must be between 0 and 100.".into());
        }
        if self.form.postgres_port == 0 {
            return Err("Postgres port must be greater than zero.".into());
        }
        Ok(())
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_tests();
        self.render_sidebar(ctx);
        self.render_footer(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(30, 26)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let tab = self.current_tab;
                        let changed = match tab {
                            SettingsTab::Identity => render_identity_tab(ui, &mut self.form),
                            SettingsTab::Capture => render_capture_tab(ui, &mut self.form),
                            SettingsTab::Minio => render_minio_tab(ui, &mut self.form),
                            SettingsTab::Postgres => render_postgres_tab(ui, &mut self.form),
                            SettingsTab::Analysis => render_analysis_tab(ui, &mut self.form),
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
                    .fill(theme::PANEL_HI)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                // Header: logo + product name.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    theme::logo(ui, &self.logo, 28.0, egui::Color32::WHITE);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            egui::RichText::new("CI-Applet")
                                .font(theme::light(16.0))
                                .color(theme::TEXT),
                        );
                        ui.label(
                            egui::RichText::new("Settings")
                                .font(theme::body(11.0))
                                .color(theme::MUTED),
                        );
                    });
                });
                ui.add_space(14.0);

                // Nav items — advanced (legacy) connections stay hidden unless
                // the user opts in below.
                for tab in SettingsTab::all() {
                    if tab.is_advanced() && !self.show_advanced {
                        continue;
                    }
                    let selected = self.current_tab == tab;
                    if theme::nav_item(ui, tab.accent(), tab.label(), tab.subtitle(), selected) {
                        self.current_tab = tab;
                    }
                    ui.add_space(3.0);
                }

                // Advanced toggle: reveals the legacy Storage/Database tabs.
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if theme::toggle(ui, &mut self.show_advanced).changed()
                        && !self.show_advanced
                        && self.current_tab.is_advanced()
                    {
                        // Hid the tab we were on — fall back to Identity.
                        self.current_tab = SettingsTab::Identity;
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            egui::RichText::new("Advanced")
                                .font(theme::body(12.5))
                                .color(theme::TEXT),
                        );
                        ui.label(
                            egui::RichText::new("Legacy storage & database")
                                .font(theme::body(10.5))
                                .color(theme::MUTED),
                        );
                    });
                });

                // Push the connection card to the bottom.
                let (text, sub, color) = self.connection_status();
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    theme::card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            theme::status_dot(ui, color, true);
                            ui.label(
                                egui::RichText::new(text)
                                    .font(theme::semibold(12.5))
                                    .color(theme::TEXT),
                            );
                        });
                        ui.add_space(5.0);
                        theme::caption(ui, sub);
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for c in [theme::LANE_SHOT, theme::LANE_ACTIVITY, theme::LANE_FILE] {
                                let (r, _) =
                                    ui.allocate_exact_size(egui::Vec2::splat(7.0), egui::Sense::hover());
                                ui.painter().circle_filled(r.center(), 3.5, c);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| theme::mono_label(ui, "3 lanes", theme::FAINT),
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
                    .fill(theme::PANEL_HI)
                    .inner_margin(egui::Margin::symmetric(22, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(error) = self.error.clone() {
                        theme::status_dot(ui, theme::RED, false);
                        ui.label(
                            egui::RichText::new(error)
                                .font(theme::body(11.5))
                                .color(theme::RED),
                        );
                    } else {
                        theme::mono_label(ui, &config_path, theme::FAINT);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        if theme::primary_button(ui, "Save").clicked() {
                            self.save(ctx);
                        }
                        if theme::ghost_button(ui, "Close").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        clicked = theme::ghost_button(ui, "Test connection").clicked();
        match state {
            Some(TestState::Running) => {
                ui.add(egui::Spinner::new().size(16.0).color(theme::AMBER));
                ui.label(
                    egui::RichText::new("Testing…")
                        .font(theme::body(12.0))
                        .color(theme::AMBER),
                );
            }
            Some(TestState::Ok(_)) => {
                theme::status_badge(ui, "Connected", theme::GREEN);
            }
            Some(TestState::Err(_)) => {
                theme::status_badge(ui, "Failed", theme::RED);
            }
            None => {}
        }
    });
    if let Some(TestState::Ok(detail) | TestState::Err(detail)) = state {
        ui.add_space(7.0);
        theme::mono_label(ui, detail, theme::MUTED);
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

fn render_identity_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        "Identity",
        "Where captures are sent, and how the applet authenticates.",
    );

    changed |= field(
        ui,
        "CI-Server URL",
        &mut form.portal_url,
        "https://your-ci-server.example.com",
        theme::FieldOpts::monospace(),
    );

    theme::field_label(ui, "Authentication");
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::INPUT)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if theme::segment(ui, "API key", form.auth_mode == AuthMode::ApiKey) {
                    form.auth_mode = AuthMode::ApiKey;
                    changed = true;
                }
                if theme::segment(ui, "OIDC browser login", form.auth_mode == AuthMode::Oidc) {
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
                "API Key",
                &mut form.applet_auth_token,
                "Paste CI-Server API key",
                theme::FieldOpts::password(),
            );
            theme::caption(
                ui,
                "Sent as x-api-key. Pasting a key clears any OIDC session.",
            );
            ui.add_space(15.0);
        }
        AuthMode::Oidc => {
            changed |= field(
                ui,
                "OIDC Issuer URL",
                &mut form.oidc_issuer,
                "https://your-ci-server.example.com/api/auth",
                theme::FieldOpts::monospace(),
            );
            ui.columns(2, |cols| {
                changed |= field(
                    &mut cols[0],
                    "Client ID",
                    &mut form.oidc_client_id,
                    "ci-applet",
                    theme::FieldOpts::monospace(),
                );
                changed |= field(
                    &mut cols[1],
                    "Scopes",
                    &mut form.oidc_scopes,
                    "openid profile email",
                    theme::FieldOpts::monospace(),
                );
            });
            changed |= field(
                ui,
                "Redirect URI",
                &mut form.oidc_redirect_uri,
                "http://127.0.0.1:49473/callback",
                theme::FieldOpts::monospace(),
            );
        }
    }

    ui.add_space(3.0);
    theme::field_label(ui, "Advanced identity");
    ui.add_space(10.0);
    ui.columns(2, |cols| {
        cols[0].label(
            egui::RichText::new("User ID override")
                .font(theme::body(11.5))
                .color(theme::MUTED),
        );
        cols[0].add_space(5.0);
        changed |= theme::text_field(
            &mut cols[0],
            &mut form.user_id,
            "User UUID",
            theme::FieldOpts::monospace(),
        );

        cols[1].label(
            egui::RichText::new("Device ID (auto)")
                .font(theme::body(11.5))
                .color(theme::MUTED),
        );
        cols[1].add_space(5.0);
        if form.device_id.is_empty() {
            theme::mono_label(&mut cols[1], "set after login / save", theme::FAINT);
        } else {
            theme::mono_label(&mut cols[1], &form.device_id, theme::TEXT);
        }
    });
    changed
}

fn normalize_ci_server_url(raw: &str) -> Result<String, String> {
    let mut url = Url::parse(raw).map_err(|_| "CI-Server URL must be a valid URL.".to_string())?;
    url.set_query(None);
    url.set_fragment(None);

    let path = url.path().trim_matches('/');
    if path.is_empty() || path == "feed" || path.starts_with("feed/") {
        url.set_path("");
    }

    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{AuthMode, SettingsFormState, normalize_ci_server_url};
    use crate::config::{AppConfig, OidcSession};

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
            normalize_ci_server_url("https://ci-server.localhost/feed").unwrap(),
            "https://ci-server.localhost/"
        );
        assert_eq!(
            normalize_ci_server_url("https://ci.example.com/feed?cursor=abc").unwrap(),
            "https://ci.example.com/"
        );
    }

    #[test]
    fn api_base_path_is_preserved() {
        assert_eq!(
            normalize_ci_server_url("https://ci.example.com/api").unwrap(),
            "https://ci.example.com/api"
        );
    }
}

fn render_capture_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        "Capture",
        "How often the desktop is perceived, and the diff gate that skips near-identical frames.",
    );

    ui.columns(2, |cols| {
        // Screenshot interval stepper.
        theme::card(&mut cols[0], |ui| {
            ui.set_width(ui.available_width());
            theme::field_label(ui, "Screenshot interval");
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
                                    .font(theme::light(30.0))
                                    .color(theme::TEXT),
                            );
                            ui.label(
                                egui::RichText::new("s")
                                    .font(theme::body(14.0))
                                    .color(theme::MUTED),
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
            ui.vertical_centered(|ui| theme::caption(ui, "Floored to 30s · default 300s"));
        });

        // Diff threshold slider.
        theme::card(&mut cols[1], |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::field_label(ui, "Diff threshold");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("%")
                            .font(theme::body(13.0))
                            .color(theme::TEAL),
                    );
                    ui.label(
                        egui::RichText::new(format!("{:.0}", form.diff_threshold_percent))
                            .font(theme::light(22.0))
                            .color(theme::TEAL),
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
            theme::caption(ui, "Frames changing less than this are skipped.");
        });
    });

    ui.add_space(20.0);
    theme::field_label(ui, "Capture lanes");
    ui.add_space(10.0);
    theme::lane_row(
        ui,
        theme::LANE_SHOT,
        "Screenshots",
        "Primary display, diff-gated & screensaver-aware",
        "vision-analyzed",
    );
    ui.add_space(10.0);
    theme::lane_row(
        ui,
        theme::LANE_ACTIVITY,
        "Activity",
        "vizier perception · app, window, filesystem deltas",
        "JSON-Patch",
    );
    ui.add_space(10.0);
    theme::lane_row(
        ui,
        theme::LANE_FILE,
        "File tracking",
        "Watched folders → Markdown · durable queue",
        "queued",
    );
    changed
}

/// A small square +/- button for the interval stepper.
fn stepper_button(ui: &mut egui::Ui, glyph: &str) -> bool {
    let btn = egui::Button::new(
        egui::RichText::new(glyph)
            .font(theme::body(18.0))
            .color(theme::TEXT),
    )
    .fill(theme::INPUT)
    .stroke(egui::Stroke::new(1.0, theme::BORDER_HI))
    .corner_radius(egui::CornerRadius::same(9))
    .min_size(egui::Vec2::splat(34.0));
    ui.add(btn).clicked()
}

fn render_minio_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        "Storage",
        "MinIO / S3-compatible object store for captured artifacts.",
    );

    changed |= field(
        ui,
        "Endpoint",
        &mut form.minio_endpoint,
        "http://localhost:9000",
        theme::FieldOpts::monospace(),
    );
    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            "Bucket",
            &mut form.minio_bucket,
            "ci-captures",
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            "Region",
            &mut form.minio_region,
            "us-east-1",
            theme::FieldOpts::monospace(),
        );
    });
    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            "Access Key",
            &mut form.minio_access_key,
            "Access key",
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            "Secret Key",
            &mut form.minio_secret_key,
            "Secret key",
            theme::FieldOpts::password(),
        );
    });
    changed |= theme::toggle_row(
        ui,
        &mut form.minio_use_ssl,
        "Use SSL",
        "Encrypt the connection to the object store.",
    );
    changed
}

fn render_postgres_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(ui, "Database", "Postgres connection for capture metadata.");

    ui.columns(2, |cols| {
        changed |= field(
            &mut cols[0],
            "Host",
            &mut form.postgres_host,
            "db.example.com",
            theme::FieldOpts::monospace(),
        );
        theme::field_label(&mut cols[1], "Port");
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
            "Database",
            &mut form.postgres_database,
            "companion",
            theme::FieldOpts::monospace(),
        );
        changed |= field(
            &mut cols[1],
            "User",
            &mut form.postgres_user,
            "ci_applet",
            theme::FieldOpts::monospace(),
        );
    });
    changed |= field(
        ui,
        "Password",
        &mut form.postgres_password,
        "Password",
        theme::FieldOpts::password(),
    );
    changed |= theme::toggle_row(
        ui,
        &mut form.postgres_require_ssl,
        "Require SSL",
        "Refuse unencrypted database connections.",
    );
    changed
}

fn render_analysis_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    page_header(
        ui,
        "Vision",
        "Optional local vision model. Leave blank to let CI-Server describe captures; \
         set an OpenAI-compatible endpoint (e.g. LM Studio or Ollama) to analyse them on this machine.",
    );

    // The client POSTs `{endpoint}/chat/completions` with Bearer auth (OpenAI
    // protocol), so the endpoint is a base URL — NOT an Anthropic /messages URL.
    changed |= field(
        ui,
        "Endpoint",
        &mut form.analysis_endpoint,
        "http://localhost:1234/v1",
        theme::FieldOpts::monospace(),
    );
    changed |= field(
        ui,
        "API Key",
        &mut form.analysis_api_key,
        "lm-studio (local endpoints often ignore this)",
        theme::FieldOpts::password(),
    );
    changed |= field(
        ui,
        "Model",
        &mut form.analysis_model,
        "lmstudio-community/vision-large",
        theme::FieldOpts::monospace(),
    );
    changed
}
