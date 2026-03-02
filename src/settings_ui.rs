use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use eframe::{NativeOptions, egui};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::config::{AppConfig, ConfigManager, MinioConfig, PostgresConfig};

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
        .with_inner_size([520.0, 640.0])
        .with_min_inner_size([420.0, 520.0]);

    eframe::run_native(
        "CI-Applet Settings",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(SettingsApp::new(
                manager_clone.clone(),
                base_clone.clone(),
                saved_clone.clone(),
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
}

impl From<&AppConfig> for SettingsFormState {
    fn from(config: &AppConfig) -> Self {
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
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
            SettingsTab::Minio => "MinIO",
            SettingsTab::Postgres => "Postgres",
            SettingsTab::Analysis => "Vision",
        }
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
}

impl SettingsApp {
    fn new(
        manager: Arc<ConfigManager>,
        base_config: Arc<Mutex<AppConfig>>,
        saved_flag: Arc<Mutex<bool>>,
    ) -> Self {
        let form = {
            let cfg = base_config.lock();
            SettingsFormState::from(&*cfg)
        };
        Self {
            manager,
            base_config,
            form,
            current_tab: SettingsTab::Identity,
            error: None,
            saved_flag,
        }
    }

    fn save(&mut self, ctx: &egui::Context) {
        if let Err(message) = self.validate() {
            self.error = Some(message);
            return;
        }

        let mut updated = self.base_config.lock().clone();
        let user_id = Uuid::parse_str(self.form.user_id.trim()).unwrap();
        updated.identity.user_id = Some(user_id);
        updated.identity.device_id = None;
        updated.server.portal_url = self.form.portal_url.trim().to_string();
        updated.capture.screenshot_interval_secs = self.form.screenshot_interval_secs;
        updated.capture.diff_threshold_percent = self.form.diff_threshold_percent;
        updated.minio = MinioConfig {
            endpoint: self.form.minio_endpoint.clone(),
            bucket: self.form.minio_bucket.clone(),
            region: self.form.minio_region.clone(),
            access_key: self.form.minio_access_key.clone(),
            secret_key: self.form.minio_secret_key.clone(),
            use_ssl: self.form.minio_use_ssl,
        };
        updated.postgres = PostgresConfig {
            host: self.form.postgres_host.clone(),
            port: self.form.postgres_port,
            user: self.form.postgres_user.clone(),
            password: self.form.postgres_password.clone(),
            database: self.form.postgres_database.clone(),
            require_ssl: self.form.postgres_require_ssl,
        };
        updated.analysis.endpoint = self.form.analysis_endpoint.clone();
        updated.analysis.api_key = self.form.analysis_api_key.clone();
        updated.analysis.model = self.form.analysis_model.clone();

        if let Err(err) = self.manager.save(&updated) {
            self.error = Some(format!("Failed to save config: {err}"));
            return;
        }

        *self.base_config.lock() = updated;
        *self.saved_flag.lock() = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn validate(&self) -> Result<(), String> {
        if self.form.user_id.trim().is_empty() {
            return Err("User ID is required.".into());
        }
        Uuid::parse_str(self.form.user_id.trim())
            .map_err(|_| "User ID must be a valid UUID.".to_string())?;
        if self.form.portal_url.trim().is_empty() {
            return Err("Portal URL cannot be empty.".into());
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
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("CI-Applet Settings");
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(208, 17, 54), error);
                ui.add_space(8.0);
            }

            ui.horizontal(|ui| {
                for tab in SettingsTab::all() {
                    let selected = self.current_tab == tab;
                    if ui.selectable_label(selected, tab.label()).clicked() {
                        self.current_tab = tab;
                    }
                }
            });
            ui.separator();

            let changed = match self.current_tab {
                SettingsTab::Identity => render_identity_tab(ui, &mut self.form),
                SettingsTab::Capture => render_capture_tab(ui, &mut self.form),
                SettingsTab::Minio => render_minio_tab(ui, &mut self.form),
                SettingsTab::Postgres => render_postgres_tab(ui, &mut self.form),
                SettingsTab::Analysis => render_analysis_tab(ui, &mut self.form),
            };
            if changed {
                self.error = None;
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Save").clicked() {
                    self.save(ctx);
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

fn render_identity_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    ui.label("User ID (UUID)");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.user_id).hint_text("User UUID"))
        .changed();
    ui.label("Portal Login URL");
    changed |= ui
        .add(
            egui::TextEdit::singleline(&mut form.portal_url)
                .hint_text("https://dev.companionintelligence.com/login"),
        )
        .changed();
    ui.separator();
    ui.label("Device ID (auto-assigned)");
    if form.device_id.is_empty() {
        ui.label(egui::RichText::new("Device will register after saving user ID").italics());
    } else {
        ui.monospace(&form.device_id);
    }
    changed
}

fn render_capture_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    ui.label("Screenshot interval (seconds)");
    changed |= ui
        .add(egui::DragValue::new(&mut form.screenshot_interval_secs).range(30..=86_400))
        .changed();
    ui.label("Diff threshold (%)");
    changed |= ui
        .add(egui::Slider::new(
            &mut form.diff_threshold_percent,
            0.0..=100.0,
        ))
        .changed();
    changed
}

fn render_minio_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    ui.label("Endpoint");
    changed |= ui
        .add(
            egui::TextEdit::singleline(&mut form.minio_endpoint).hint_text("http://localhost:9000"),
        )
        .changed();
    ui.label("Bucket");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.minio_bucket).hint_text("Bucket"))
        .changed();
    ui.label("Region");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.minio_region).hint_text("Region"))
        .changed();
    ui.label("Access Key");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.minio_access_key).hint_text("Access key"))
        .changed();
    ui.label("Secret Key");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.minio_secret_key).hint_text("Secret key"))
        .changed();
    changed |= ui.checkbox(&mut form.minio_use_ssl, "Use SSL").changed();
    changed
}

fn render_postgres_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    ui.label("Host");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.postgres_host).hint_text("Host"))
        .changed();
    ui.label("Port");
    changed |= ui
        .add(egui::DragValue::new(&mut form.postgres_port).range(1..=65535))
        .changed();
    ui.label("Database");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.postgres_database).hint_text("Database"))
        .changed();
    ui.label("User");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.postgres_user).hint_text("User"))
        .changed();
    ui.label("Password");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.postgres_password).hint_text("Password"))
        .changed();
    changed |= ui
        .checkbox(&mut form.postgres_require_ssl, "Require SSL")
        .changed();
    changed
}

fn render_analysis_tab(ui: &mut egui::Ui, form: &mut SettingsFormState) -> bool {
    let mut changed = false;
    ui.label("Vision Endpoint");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.analysis_endpoint).hint_text("Vision endpoint"))
        .changed();
    ui.label("API Key");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.analysis_api_key).hint_text("API key"))
        .changed();
    ui.label("Model");
    changed |= ui
        .add(egui::TextEdit::singleline(&mut form.analysis_model).hint_text("Model"))
        .changed();
    changed
}
