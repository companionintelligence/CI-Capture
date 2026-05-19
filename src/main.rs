mod analysis;
mod capture;
mod config;
mod conversion;
mod db;
mod extractors;
mod minio;
mod server_auth;
mod server_common;
mod server_ingest;
mod server_upload;
mod settings_ui;
mod svg_icon;
mod upload_ui;
mod worker;

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use config::{AppConfig, ConfigManager, PostgresConfig};
use env_logger::Env;
use parking_lot::Mutex;
use server_auth::{start_pairing, wait_for_pairing};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tinyfiledialogs::{self as dialogs, MessageBoxIcon};
use tokio::runtime::Runtime;
use tokio_postgres::NoTls;
use tray_icon::menu::{MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;
use whoami;

use crate::worker::WorkerHandle;
use settings_ui::run_settings_ui;
use upload_ui::run_upload_ui;

#[derive(Debug, Clone)]
enum AppEvent {
    ReloadConfig,
    PairingFinished { success: bool, message: String },
}

fn main() -> Result<()> {
    let env = Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env).init();

    if std::env::args().any(|arg| arg == "--settings-ui") {
        run_settings_ui()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--upload-ui") {
        run_upload_ui()?;
        return Ok(());
    }

    let manager = ConfigManager::new().context("building config manager")?;
    let mut config = manager
        .load_or_default()
        .context("loading persisted configuration")?;
    ensure_device_registration(&manager, &mut config)?;
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let initial_config = config;

    let worker = WorkerHandle::spawn(initial_config.clone());
    let menu_state = MenuState::new(&initial_config).context("building tray menu")?;
    menu_state.refresh_labels(&initial_config, worker.is_running());

    let icon = build_icon();
    let tray_menu = menu_state.menu.clone();
    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("CI-Applet")
        .with_icon(icon)
        .with_icon_as_template(true)
        .build()
        .context("creating tray icon")?;

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let mut controller = AppController {
        shared_config,
        manager,
        worker,
        menu_state,
        tray_icon: Some(tray_icon),
        proxy,
        settings_open: false,
        pairing_in_progress: false,
    };

    let menu_receiver = MenuEvent::receiver().clone();
    let tray_receiver = TrayIconEvent::receiver().clone();

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::MainEventsCleared => {
                while let Ok(event) = menu_receiver.try_recv() {
                    if controller.handle_menu_event(event, target, control_flow) {
                        return;
                    }
                }
                while let Ok(event) = tray_receiver.try_recv() {
                    controller.handle_tray_event(event);
                }
            }
            Event::UserEvent(app_event) => {
                controller.handle_app_event(app_event, target);
            }
            Event::LoopDestroyed => {
                controller.worker.shutdown();
            }
            _ => {}
        }
    });
}

struct AppController {
    shared_config: Arc<Mutex<AppConfig>>,
    manager: ConfigManager,
    worker: WorkerHandle,
    menu_state: MenuState,
    tray_icon: Option<TrayIcon>,
    proxy: EventLoopProxy<AppEvent>,
    settings_open: bool,
    pairing_in_progress: bool,
}

impl AppController {
    fn handle_menu_event(
        &mut self,
        event: MenuEvent,
        _target: &EventLoopWindowTarget<AppEvent>,
        control_flow: &mut ControlFlow,
    ) -> bool {
        let menu_id = event.id.clone();
        if menu_id == *self.menu_state.toggle_item.id() {
            self.toggle_worker();
        } else if menu_id == *self.menu_state.capture_now_item.id() {
            self.worker.capture_now();
        } else if menu_id == *self.menu_state.upload_item.id() {
            self.open_upload_window();
        } else if menu_id == *self.menu_state.settings_item.id() {
            self.open_settings_window();
        } else if menu_id == *self.menu_state.login_item.id() {
            self.launch_portal_login();
        } else if menu_id == *self.menu_state.show_config_item.id() {
            self.show_config_path();
        } else if menu_id == *self.menu_state.quit_item.id() {
            self.shutdown();
            *control_flow = ControlFlow::Exit;
            return true;
        }
        false
    }

    fn handle_tray_event(&mut self, event: TrayIconEvent) {
        log::debug!("tray event: {event:?}");
    }

    fn handle_app_event(&mut self, event: AppEvent, _target: &EventLoopWindowTarget<AppEvent>) {
        match event {
            AppEvent::ReloadConfig => {
                self.settings_open = false;
                if let Err(err) = self.reload_config_from_disk() {
                    log::error!("failed to reload config: {err:?}");
                    show_error("Unable to reload configuration; see logs for details.");
                }
            }
            AppEvent::PairingFinished { success, message } => {
                self.pairing_in_progress = false;
                if success {
                    if let Err(err) = self.reload_config_from_disk() {
                        log::error!("failed to reload config after pairing: {err:?}");
                        show_error("Linked account, but failed to reload configuration.");
                        return;
                    }
                    dialogs::message_box_ok("CI-Applet", &message, MessageBoxIcon::Info);
                } else {
                    show_error(&message);
                }
            }
        }
    }

    fn open_settings_window(&mut self) {
        if self.settings_open {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            show_error("Unable to locate executable for settings window.");
            return;
        };
        self.settings_open = true;
        let proxy = self.proxy.clone();
        match Command::new(exe).arg("--settings-ui").spawn() {
            Ok(mut child) => {
                thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        log::error!("settings process failed: {err:?}");
                    }
                    let _ = proxy.send_event(AppEvent::ReloadConfig);
                });
            }
            Err(err) => {
                self.settings_open = false;
                log::error!("failed to launch settings window: {err:?}");
                show_error("Unable to launch settings window. Check logs for details.");
            }
        }
    }

    fn open_upload_window(&self) {
        let Ok(exe) = std::env::current_exe() else {
            show_error("Unable to locate executable for upload window.");
            return;
        };
        if let Err(err) = Command::new(exe).arg("--upload-ui").spawn() {
            log::error!("failed to launch upload window: {err:?}");
            show_error("Unable to launch upload window. Check logs for details.");
        }
    }

    fn toggle_worker(&mut self) {
        if self.worker.is_running() {
            self.worker.pause();
        } else {
            self.worker.resume();
        }
        let snapshot = self.current_config();
        self.menu_state
            .refresh_labels(&snapshot, self.worker.is_running());
    }

    fn launch_portal_login(&mut self) {
        if self.pairing_in_progress {
            show_error("Portal login is already in progress.");
            return;
        }

        let snapshot = self.current_config();
        let portal_url = snapshot.server.portal_url.trim().to_string();
        if portal_url.is_empty() {
            show_error("Portal URL is not configured.");
            return;
        }

        self.pairing_in_progress = true;

        let proxy = self.proxy.clone();
        let portal_url_for_error = portal_url.clone();
        thread::spawn(move || {
            let result = pair_with_portal(&portal_url);
            let event = match result {
                Ok(message) => AppEvent::PairingFinished {
                    success: true,
                    message,
                },
                Err(err) => {
                    log::error!("portal pairing failed: {err:?}");
                    AppEvent::PairingFinished {
                        success: false,
                        message: format!(
                            "Unable to complete portal login against {portal_url_for_error}: {err}. \
Check Settings > Identity and point Portal URL at the CI-Server web origin that serves both /applet/pair and /api/graphql."
                        ),
                    }
                }
            };
            let _ = proxy.send_event(event);
        });
    }

    fn show_config_path(&self) {
        let path = self.manager.path().display().to_string();
        dialogs::message_box_ok("Config Path", &path, MessageBoxIcon::Info);
    }

    fn shutdown(&mut self) {
        self.worker.shutdown();
        if let Some(icon) = self.tray_icon.take() {
            drop(icon);
        }
    }

    fn current_config(&self) -> AppConfig {
        self.shared_config.lock().clone()
    }

    fn reload_config_from_disk(&mut self) -> Result<()> {
        let mut new_config = self
            .manager
            .load_or_default()
            .context("reloading configuration")?;
        ensure_device_registration(&self.manager, &mut new_config)?;
        {
            let mut guard = self.shared_config.lock();
            *guard = new_config.clone();
        }
        self.worker.update_config(new_config.clone());
        self.menu_state
            .refresh_labels(&new_config, self.worker.is_running());
        Ok(())
    }
}

struct MenuState {
    menu: tray_icon::menu::Menu,
    status_item: MenuItem,
    toggle_item: MenuItem,
    capture_now_item: MenuItem,
    upload_item: MenuItem,
    interval_item: MenuItem,
    threshold_item: MenuItem,
    identity_item: MenuItem,
    server_item: MenuItem,
    login_item: MenuItem,
    settings_item: MenuItem,
    postgres_item: MenuItem,
    minio_item: MenuItem,
    analysis_item: MenuItem,
    show_config_item: MenuItem,
    quit_item: MenuItem,
}

impl MenuState {
    fn new(config: &AppConfig) -> Result<Self> {
        let menu = tray_icon::menu::Menu::new();
        let status_item = MenuItem::new("Status: Starting", false, None);
        status_item.set_enabled(false);
        let toggle_item = MenuItem::new("Pause Captures", true, None);
        let capture_now_item = MenuItem::new("Capture Now", true, None);
        let upload_item = MenuItem::new("Upload Document…", true, None);
        let interval_item = MenuItem::new("Screenshot Interval", true, None);
        let threshold_item = MenuItem::new("Diff Threshold", true, None);
        let identity_item = MenuItem::new("Set Identity IDs", true, None);
        let server_item = MenuItem::new("Portal URL", true, None);
        let login_item = MenuItem::new("Login to Portal", true, None);
        let settings_item = MenuItem::new("Open Settings…", true, None);
        let postgres_item = MenuItem::new("Configure Postgres", true, None);
        let minio_item = MenuItem::new("Configure MinIO", true, None);
        let analysis_item = MenuItem::new("Configure Vision Endpoint", true, None);
        let show_config_item = MenuItem::new("Show Config Location", true, None);
        let quit_item = MenuItem::new("Quit CI-Applet", true, None);

        interval_item.set_enabled(false);
        threshold_item.set_enabled(false);
        identity_item.set_enabled(false);
        server_item.set_enabled(false);
        postgres_item.set_enabled(false);
        minio_item.set_enabled(false);
        analysis_item.set_enabled(false);

        menu.append_items(&[
            &status_item,
            &PredefinedMenuItem::separator(),
            &toggle_item,
            &capture_now_item,
            &upload_item,
            &PredefinedMenuItem::separator(),
            &interval_item,
            &threshold_item,
            &identity_item,
            &server_item,
            &login_item,
            &postgres_item,
            &minio_item,
            &analysis_item,
            &PredefinedMenuItem::separator(),
            &settings_item,
            &show_config_item,
            &quit_item,
        ])?;

        let state = Self {
            menu,
            status_item,
            toggle_item,
            capture_now_item,
            upload_item,
            interval_item,
            threshold_item,
            identity_item,
            server_item,
            login_item,
            settings_item,
            postgres_item,
            minio_item,
            analysis_item,
            show_config_item,
            quit_item,
        };
        state.refresh_labels(config, true);
        Ok(state)
    }

    fn refresh_labels(&self, config: &AppConfig, running: bool) {
        let status = if running { "Monitoring" } else { "Paused" };
        self.status_item.set_text(&format!("Status: {status}"));
        self.toggle_item.set_text(if running {
            "Pause Captures"
        } else {
            "Resume Captures"
        });
        self.interval_item.set_text(&format!(
            "Screenshot Interval: {}s",
            config.capture.screenshot_interval_secs
        ));
        self.threshold_item.set_text(&format!(
            "Diff Threshold: {:.1}%",
            config.capture.diff_threshold_percent
        ));
        self.identity_item.set_text(&format!(
            "Identity: user {} / device {}",
            short_uuid(config.identity.user_id),
            short_uuid(config.identity.device_id)
        ));
        self.server_item
            .set_text(&format!("Portal URL: {}", config.server.portal_url));
        let host_display = config
            .server
            .portal_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        self.login_item
            .set_text(&format!("Login to {}", host_display));
        self.postgres_item.set_text(&format!(
            "Postgres: {}:{} / {}",
            config.postgres.host, config.postgres.port, config.postgres.database
        ));
        self.minio_item.set_text(&format!(
            "MinIO: {} @ {}",
            config.minio.bucket, config.minio.endpoint
        ));
        self.analysis_item
            .set_text(&format!("Vision Model: {}", config.analysis.model));
    }
}

fn build_icon() -> Icon {
    #[cfg(target_os = "macos")]
    const ICON_SIZE: u32 = 36;
    #[cfg(not(target_os = "macos"))]
    const ICON_SIZE: u32 = 22;
    const ICON_BYTES: &[u8] = include_bytes!("../2024 CI Logo Mark.svg");

    match svg_icon::load_svg_icon(ICON_BYTES, ICON_SIZE) {
        Ok(icon) => icon,
        Err(err) => {
            log::warn!(
                "Failed to load embedded tray icon SVG: {err}. Falling back to default vector icon"
            );
            fallback_icon()
        }
    }
}

fn fallback_icon() -> Icon {
    const SIZE: u32 = 18;
    let mut data = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let highlight = if x == y || x + y == SIZE - 1 {
                255
            } else {
                180
            };
            data.extend_from_slice(&[92, 141, 255, highlight]);
        }
    }
    Icon::from_rgba(data, SIZE, SIZE).expect("invalid icon data")
}

fn show_error(message: &str) {
    dialogs::message_box_ok("CI-Applet", message, MessageBoxIcon::Error);
}

fn short_uuid(id: Option<Uuid>) -> String {
    match id {
        Some(uuid) => uuid
            .to_string()
            .split('-')
            .next()
            .unwrap_or("?")
            .to_string(),
        None => "unset".to_string(),
    }
}

fn ensure_device_registration(manager: &ConfigManager, config: &mut AppConfig) -> Result<()> {
    let Some(user_id) = config.identity.user_id else {
        config.identity.device_id = None;
        return Ok(());
    };
    if config.identity.device_id.is_some() {
        return Ok(());
    }

    let hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string());
    let device_name = format!("ci-applet-{}", hostname);
    let platform = std::env::consts::OS.to_string();
    let model = whoami::devicename();
    let os_version = whoami::distro();
    let app_version = env!("CARGO_PKG_VERSION").to_string();
    let postgres = config.postgres.clone();

    let runtime = Runtime::new().context("creating runtime for device registration")?;
    let device_id = runtime.block_on(async move {
        register_or_create_device(
            postgres,
            user_id,
            device_name,
            platform,
            model,
            os_version,
            app_version,
        )
        .await
    })?;

    config.identity.device_id = Some(device_id);
    manager
        .save(config)
        .context("saving config after device registration")?;
    Ok(())
}

async fn register_or_create_device(
    postgres: PostgresConfig,
    user_id: Uuid,
    device_name: String,
    platform: String,
    model: String,
    os_version: String,
    app_version: String,
) -> Result<Uuid> {
    let mut pg = tokio_postgres::Config::new();
    pg.host(&postgres.host);
    pg.port(postgres.port);
    pg.user(&postgres.user);
    pg.password(&postgres.password);
    pg.dbname(&postgres.database);
    pg.application_name("ci-Applet");

    let (client, connection) = pg.connect(NoTls).await.context("connecting to postgres")?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            log::error!("device registry connection error: {err}");
        }
    });

    if let Some(row) = client
        .query_opt(
            "SELECT device_id FROM ci.devices WHERE user_id=$1 AND device_name=$2 LIMIT 1",
            &[&user_id, &device_name],
        )
        .await
        .context("querying existing device")?
    {
        return Ok(row.get("device_id"));
    }

    let device_type = "desktop";
    let row = client
        .query_one(
            "INSERT INTO ci.devices (
                user_id,
                device_name,
                device_type,
                platform,
                model,
                os_version,
                app_version,
                last_seen,
                is_active
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,NOW(),TRUE)
            RETURNING device_id",
            &[
                &user_id,
                &device_name,
                &device_type,
                &platform,
                &model,
                &os_version,
                &app_version,
            ],
        )
        .await
        .context("inserting device")?;
    Ok(row.get("device_id"))
}

fn pair_with_portal(portal_url: &str) -> Result<String> {
    let runtime = Runtime::new().context("creating runtime for portal pairing")?;

    let launch = runtime.block_on(async { start_pairing(portal_url).await })?;
    if let Err(err) = Command::new("open").arg(&launch.pairing_url).status() {
        return Err(anyhow!("failed to open pairing URL: {err}"));
    }

    let completion = runtime.block_on(async {
        wait_for_pairing(portal_url, &launch, Duration::from_secs(15 * 60)).await
    })?;

    let manager = ConfigManager::new().context("reloading config manager after pairing")?;
    let mut config = manager
        .load_or_default()
        .context("loading config after pairing")?;
    config.identity.user_id = Some(completion.user_id);
    config.identity.device_id = Some(completion.device_id);
    config.server.applet_auth_token = Some(completion.applet_auth_token);
    manager
        .save(&config)
        .context("saving config after portal pairing")?;

    let display_name = completion
        .user_name
        .or(completion.user_email)
        .unwrap_or_else(|| completion.user_id.to_string());

    Ok(format!(
        "Linked this computer to {display_name}. CI-Applet will now attribute captures to that account."
    ))
}

