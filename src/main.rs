mod analysis;
mod capture;
mod capture_store;
mod config;
mod connection_test;
mod conversion;
mod db;
mod extractors;
mod file_tracker;
#[cfg(feature = "legacy-minio")]
mod minio;
mod oidc;
mod onboarding_ui;
mod permissions;
mod queue;
mod server_check;
mod server_common;
mod server_ingest;
mod server_upload;
mod settings_ui;
mod svg_icon;
mod sync;
mod theme;
mod upload_ui;
mod watcher;
mod worker;

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use config::{AppConfig, ConfigManager};
use env_logger::Env;
use parking_lot::Mutex;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tinyfiledialogs::{self as dialogs, MessageBoxIcon};
use tokio::runtime::Runtime;
use tray_icon::menu::{MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;
use whoami;

use crate::worker::WorkerHandle;
use onboarding_ui::run_onboarding_ui;
use settings_ui::run_settings_ui;
use upload_ui::run_upload_ui;

#[derive(Debug, Clone)]
enum AppEvent {
    ReloadConfig,
    OnboardingClosed,
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

    if std::env::args().any(|arg| arg == "--onboarding-ui") {
        run_onboarding_ui()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--check-server") {
        server_check::run(parse_portal_override())?;
        return Ok(());
    }

    // Headless portal sign-in: runs the OIDC Authorization Code + PKCE flow
    // (opens the system browser, captures the loopback redirect), stores the
    // tokens, and prints the resolved identity. Useful for scripted setup and
    // for server/CI installs that have no tray. The interactive password entry
    // happens in the browser — this command never handles the password.
    if std::env::args().any(|arg| arg == "--login") {
        match login_with_portal() {
            Ok(message) => {
                println!("{message}");
                print_login_summary();
                return Ok(());
            }
            Err(err) => {
                eprintln!("Portal sign-in failed: {err:#}");
                std::process::exit(1);
            }
        }
    }

    let manager = ConfigManager::new().context("building config manager")?;
    let config = manager
        .load_or_default()
        .context("loading persisted configuration")?;
    let shared_config = Arc::new(Mutex::new(config.clone()));
    let initial_config = config;

    let worker = WorkerHandle::spawn(initial_config.clone());
    spawn_token_refresh(worker.clone(), Arc::clone(&shared_config));

    // Build the event loop first: on Linux `tao` initializes GTK here, and the
    // tray menu/icon below are GTK objects that panic ("GTK has not been
    // initialized") if constructed beforehand. macOS doesn't route the tray
    // through GTK, so this ordering is Linux-critical but harmless there.
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

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

    let mut controller = AppController {
        shared_config,
        manager,
        worker,
        menu_state,
        tray_icon: Some(tray_icon),
        proxy,
        settings_open: false,
        onboarding_open: false,
        pairing_in_progress: false,
    };

    // First run: no credential yet and onboarding never completed → open the
    // setup assistant so the user isn't left with an invisible, unconfigured
    // menu-bar item.
    if is_first_run(&initial_config) {
        controller.open_onboarding_window();
    }

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
    onboarding_open: bool,
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
        } else if menu_id == *self.menu_state.setup_item.id() {
            self.open_onboarding_window();
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
            AppEvent::OnboardingClosed => {
                self.onboarding_open = false;
                if let Err(err) = self.reload_config_from_disk() {
                    log::error!("failed to reload config after onboarding: {err:?}");
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

    /// Open the first-run onboarding wizard (also reachable from the tray's
    /// "Setup Assistant"). Reloads config when it closes so a fresh sign-in or
    /// completed onboarding takes effect immediately.
    fn open_onboarding_window(&mut self) {
        if self.onboarding_open {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            show_error("Unable to locate executable for the setup assistant.");
            return;
        };
        self.onboarding_open = true;
        let proxy = self.proxy.clone();
        match Command::new(exe).arg("--onboarding-ui").spawn() {
            Ok(mut child) => {
                thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        log::error!("onboarding process failed: {err:?}");
                    }
                    let _ = proxy.send_event(AppEvent::OnboardingClosed);
                });
            }
            Err(err) => {
                self.onboarding_open = false;
                log::error!("failed to launch onboarding window: {err:?}");
                show_error("Unable to launch the setup assistant. Check logs for details.");
            }
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
        let issuer = snapshot.oidc.issuer.trim().to_string();
        let client_id = snapshot.oidc.client_id.clone();
        if issuer.is_empty() {
            show_error("OIDC issuer is not configured (config.json → oidc.issuer).");
            return;
        }

        self.pairing_in_progress = true;

        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let result = login_with_portal();
            let event = match result {
                Ok(message) => AppEvent::PairingFinished {
                    success: true,
                    message,
                },
                Err(err) => {
                    log::error!("portal sign-in failed: {err:?}");
                    AppEvent::PairingFinished {
                        success: false,
                        message: format!(
                            "Unable to sign in to {issuer}: {err}.\n\nCheck that the portal is reachable and that the applet's OIDC client \"{client_id}\" is registered with the loopback redirect (config.json → oidc)."
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
        let new_config = self
            .manager
            .load_or_default()
            .context("reloading configuration")?;
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
    setup_item: MenuItem,
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
        let setup_item = MenuItem::new("Setup Assistant…", true, None);
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
            &setup_item,
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
            setup_item,
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
        // Truthful status: an unconfigured or permission-blocked applet must
        // not read as "Monitoring".
        let status = if !server_ingest::is_configured(config) {
            "⚠︎ Setup needed"
        } else if permissions::permissions_relevant() && permissions::status().needs_attention() {
            "⚠︎ Permissions needed"
        } else if running {
            "Monitoring"
        } else {
            "Paused"
        };
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
        let issuer_display = config
            .oidc
            .issuer
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        self.login_item
            .set_text(&format!("Login via {}", issuer_display));
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

/// Open a URL in the user's default browser, cross-platform.
fn open_url(url: &str) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        // The empty "" is `start`'s window-title argument.
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.status()
}

/// Parse an optional `--portal <url>` / `--portal=<url>` override used by
/// `--check-server` to probe an arbitrary endpoint without mutating config.
fn parse_portal_override() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if let Some(rest) = arg.strip_prefix("--portal=") {
            return Some(rest.to_string());
        }
        if arg == "--portal" {
            return args.get(i + 1).cloned();
        }
    }
    None
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

/// Sign in to the portal via OIDC (Authorization Code + PKCE), persist the
/// resulting tokens + identity, and best-effort register this device.
/// First launch = no server credential configured and onboarding never
/// finished. Drives whether the setup assistant auto-opens.
fn is_first_run(config: &AppConfig) -> bool {
    !config.onboarding_completed && !server_ingest::is_configured(config)
}

fn login_with_portal() -> Result<String> {
    let manager = ConfigManager::new().context("loading config manager for sign-in")?;
    let oidc_config = manager
        .load_or_default()
        .context("loading config for sign-in")?
        .oidc;

    let runtime = Runtime::new().context("creating runtime for portal sign-in")?;
    let outcome = runtime
        .block_on(async {
            oidc::login(&oidc_config, |url| {
                if let Err(err) = open_url(url) {
                    log::error!("failed to open browser for sign-in: {err:?}");
                }
            })
            .await
        })
        .context("OIDC sign-in")?;

    // The interactive flow can take minutes — reload before mutating so a
    // settings save made meanwhile isn't clobbered by a stale snapshot.
    let mut config = manager
        .load_or_default()
        .context("reloading config after sign-in")?;

    let id_token = outcome.tokens.id_token.clone();
    // Prefer the id_token's own exp claim (the id_token is the ingest
    // bearer); expires_in describes the access token.
    let expires_at = outcome
        .claims
        .exp
        .and_then(|secs| chrono::DateTime::<Utc>::from_timestamp(secs, 0))
        .or_else(|| {
            outcome
                .tokens
                .expires_in
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs))
        });

    config.oidc_session = Some(config::OidcSession {
        id_token: id_token.clone(),
        access_token: Some(outcome.tokens.access_token.clone()),
        refresh_token: outcome.tokens.refresh_token.clone(),
        expires_at,
        subject: outcome.claims.sub.clone(),
        email: outcome.claims.email.clone(),
        name: outcome.claims.name.clone(),
    });

    // The id_token is the bearer CI-Server validates on ingest.
    if let Some(token) = id_token.clone() {
        config.server.applet_auth_token = Some(token);
    }

    // Map the OIDC subject onto the user id when it's a UUID.
    if let Some(sub) = &outcome.claims.sub
        && let Ok(uid) = Uuid::parse_str(sub)
    {
        config.identity.user_id = Some(uid);
    }

    // Best-effort: register this device so ingest has a server-side device id.
    if config.identity.device_id.is_none()
        && let Some(token) = id_token.as_deref()
    {
        match runtime.block_on(async { register_device(&config, token).await }) {
            Ok(device_id) => {
                config.identity.device_id = Some(device_id);
                log::info!("registered device {device_id} with CI-Server");
            }
            Err(err) => log::warn!("device registration deferred (will retry later): {err:?}"),
        }
    }

    manager
        .save(&config)
        .context("saving config after sign-in")?;

    let who = outcome
        .claims
        .name
        .or(outcome.claims.email)
        .or(outcome.claims.sub)
        .unwrap_or_else(|| "your account".to_string());
    Ok(format!(
        "Signed in as {who}. CI-Applet will now attribute captures to that account."
    ))
}

/// After `--login`, print the stored identity and the id_token's audience +
/// issuer. The audience is the value CI-Server must accept via
/// `PORTAL_OIDC_AUDIENCE`, so surfacing it makes an end-to-end auth check
/// concrete without guessing.
fn print_login_summary() {
    let Ok(manager) = ConfigManager::new() else {
        return;
    };
    let Ok(config) = manager.load_or_default() else {
        return;
    };
    if let Some(session) = &config.oidc_session {
        println!("  subject   : {}", session.subject.as_deref().unwrap_or("-"));
        println!("  email     : {}", session.email.as_deref().unwrap_or("-"));
        println!(
            "  expires   : {}",
            session
                .expires_at
                .map(|e| e.to_rfc3339())
                .unwrap_or_else(|| "-".to_string())
        );
        if let Some(token) = &session.id_token {
            match decode_jwt_aud_iss(token) {
                Some((aud, iss)) => {
                    println!("  token aud : {aud}   (CI-Server must list this in PORTAL_OIDC_AUDIENCE)");
                    println!("  token iss : {iss}");
                }
                None => println!("  id_token  : present (claims not decodable)"),
            }
        }
    }
    println!(
        "  user_id   : {}",
        config
            .identity
            .user_id
            .map(|u| u.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "  device_id : {}",
        config
            .identity
            .device_id
            .map(|u| u.to_string())
            .unwrap_or_else(|| "- (not registered)".to_string())
    );
}

/// Best-effort decode of a JWT's `aud` and `iss` claims (no signature check —
/// this is display-only diagnostics for `--login`).
fn decode_jwt_aud_iss(jwt: &str) -> Option<(String, String)> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let aud = match value.get("aud") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => "-".to_string(),
    };
    let iss = value
        .get("iss")
        .and_then(|x| x.as_str())
        .unwrap_or("-")
        .to_string();
    Some((aud, iss))
}

/// Register this machine with CI-Server (`POST /devices`, bearer-authed).
async fn register_device(config: &AppConfig, bearer: &str) -> Result<Uuid> {
    let hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string());
    let payload = serde_json::json!({
        "device_name": format!("ci-applet-{hostname}"),
        "device_type": "desktop",
        "platform": std::env::consts::OS,
        "model": whoami::devicename(),
        "os_version": whoami::distro(),
        "app_version": env!("CARGO_PKG_VERSION"),
        "is_active": true,
    });
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building device-registration client")?;

    let mut last_error = None;
    for url in server_common::devices_urls(&config.server.portal_url)? {
        match client
            .post(url.clone())
            .bearer_auth(bearer)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let v: serde_json::Value = resp.json().await.context("decoding device response")?;
                let obj = if v.is_array() {
                    v.get(0).cloned().unwrap_or(serde_json::Value::Null)
                } else {
                    v
                };
                let id_str = obj
                    .get("device_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|x| x.as_str())
                    .context("device response missing device_id")?;
                return Uuid::parse_str(id_str).context("parsing device_id");
            }
            Ok(resp)
                if matches!(
                    resp.status(),
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
                ) =>
            {
                last_error = Some(anyhow!("POST {url} → HTTP {}", resp.status()));
                continue;
            }
            Ok(resp) => return Err(anyhow!("POST {url} → HTTP {}", resp.status())),
            Err(err) => return Err(anyhow!("POST {url}: {err}")),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("device registration failed")))
}

/// How often the background loop checks token expiry / pending registration.
const TOKEN_REFRESH_CHECK: Duration = Duration::from_secs(60);

/// Failure backoff for the refresh/registration concerns: skip this many
/// ticks after `n` consecutive failures (doubling, capped at ~1 h) so a dead
/// portal, revoked grant, or unreachable server isn't hammered every minute.
fn failure_backoff_ticks(consecutive_failures: u32) -> u32 {
    if consecutive_failures == 0 {
        0
    } else {
        2u32.saturating_pow(consecutive_failures - 1).min(60)
    }
}

/// Background loop that keeps the portal session alive: refreshes the
/// `id_token` before it expires and retries device registration if it hasn't
/// happened yet. Every mutation is a **reload → merge → save** of only the
/// fields this loop owns — never a whole-config write from a stale snapshot —
/// so concurrent saves from the settings process or a fresh sign-in are
/// respected, and the atomic `ConfigManager::save` keeps the file untorn.
fn spawn_token_refresh(worker: WorkerHandle, shared: Arc<Mutex<AppConfig>>) {
    thread::spawn(move || {
        let Ok(manager) = ConfigManager::new() else {
            log::error!("token-refresh: cannot open config manager");
            return;
        };
        let runtime = match Runtime::new() {
            Ok(r) => r,
            Err(err) => {
                log::error!("token-refresh: runtime: {err:?}");
                return;
            }
        };
        let skew = chrono::Duration::minutes(2);
        let mut refresh_failures: u32 = 0;
        let mut registration_failures: u32 = 0;
        let mut skip_refresh_ticks: u32 = 0;
        let mut skip_registration_ticks: u32 = 0;
        loop {
            match token_refresh_tick(
                &manager,
                &worker,
                &shared,
                &runtime,
                skew,
                skip_refresh_ticks == 0,
                skip_registration_ticks == 0,
            ) {
                Ok(outcome) => {
                    match outcome.refresh {
                        ConcernOutcome::Succeeded => refresh_failures = 0,
                        ConcernOutcome::Failed => {
                            refresh_failures += 1;
                            skip_refresh_ticks = failure_backoff_ticks(refresh_failures);
                        }
                        ConcernOutcome::Idle => {}
                    }
                    match outcome.registration {
                        ConcernOutcome::Succeeded => registration_failures = 0,
                        ConcernOutcome::Failed => {
                            registration_failures += 1;
                            skip_registration_ticks =
                                failure_backoff_ticks(registration_failures);
                        }
                        ConcernOutcome::Idle => {}
                    }
                }
                Err(err) => log::warn!("token-refresh tick failed: {err:?}"),
            }
            thread::sleep(TOKEN_REFRESH_CHECK);
            skip_refresh_ticks = skip_refresh_ticks.saturating_sub(1);
            skip_registration_ticks = skip_registration_ticks.saturating_sub(1);
        }
    });
}

/// Per-concern result of one tick, for the caller's backoff bookkeeping.
enum ConcernOutcome {
    Idle,
    Succeeded,
    Failed,
}

struct TickOutcome {
    refresh: ConcernOutcome,
    registration: ConcernOutcome,
}

/// One iteration of the refresh loop.
#[allow(clippy::too_many_arguments)]
fn token_refresh_tick(
    manager: &ConfigManager,
    worker: &WorkerHandle,
    shared: &Arc<Mutex<AppConfig>>,
    runtime: &Runtime,
    skew: chrono::Duration,
    try_refresh: bool,
    try_registration: bool,
) -> Result<TickOutcome> {
    let mut outcome = TickOutcome {
        refresh: ConcernOutcome::Idle,
        registration: ConcernOutcome::Idle,
    };
    let config = manager.load_or_default()?;
    let Some(session) = config.oidc_session.clone() else {
        return Ok(outcome); // not signed in (API-key installs land here too)
    };

    // 1) Refresh the token if it's expiring and we hold a refresh token.
    // (An empty issuer means OIDC isn't configured — nothing to refresh
    // against.)
    if try_refresh
        && let Some(refresh_token) = session.refresh_token.clone()
        && !config.oidc.issuer.trim().is_empty()
        && oidc::token_expiring(session.expires_at, Utc::now(), skew)
    {
        let refreshed = runtime.block_on(async {
            let client = reqwest::Client::builder()
                .use_rustls_tls()
                .timeout(Duration::from_secs(30))
                .build()
                .context("building refresh client")?;
            let disco = oidc::discover(&client, &config.oidc.issuer).await?;
            oidc::refresh(&client, &disco, &config.oidc, &refresh_token).await
        });
        match refreshed {
            Ok(tokens) => {
                // Reload → merge → save: the network round-trip may have
                // taken a while, and the settings process or a fresh sign-in
                // may have written config.json meanwhile. Apply the new
                // tokens only if OUR session is still the live one.
                let mut fresh = manager.load_or_default()?;
                let still_ours = fresh
                    .oidc_session
                    .as_ref()
                    .is_some_and(|s| s.refresh_token == session.refresh_token);
                if still_ours {
                    apply_refreshed_tokens(&mut fresh, &session, tokens);
                    manager
                        .save(&fresh)
                        .context("persisting refreshed session")?;
                    *shared.lock() = fresh.clone();
                    worker.update_config(fresh);
                    outcome.refresh = ConcernOutcome::Succeeded;
                    log::info!("refreshed portal token");
                } else {
                    log::info!(
                        "discarding refreshed token: session was superseded during refresh"
                    );
                }
            }
            Err(err) => {
                outcome.refresh = ConcernOutcome::Failed;
                log::warn!("portal token refresh failed (will back off): {err:#}");
            }
        }
    }

    // 2) Retry device registration if we have a token but no device id yet.
    let config = manager.load_or_default()?;
    if try_registration
        && config.identity.device_id.is_none()
        && let Some(token) = config.server.applet_auth_token.clone()
    {
        match runtime.block_on(async { register_device(&config, &token).await }) {
            Ok(device_id) => {
                // Same reload → merge → save discipline; only fill a still-
                // empty device id.
                let mut fresh = manager.load_or_default()?;
                if fresh.identity.device_id.is_none() {
                    fresh.identity.device_id = Some(device_id);
                    manager
                        .save(&fresh)
                        .context("persisting registered device")?;
                    *shared.lock() = fresh.clone();
                    worker.update_config(fresh);
                }
                outcome.registration = ConcernOutcome::Succeeded;
                log::info!("registered device {device_id} with CI-Server (deferred)");
            }
            Err(err) => {
                outcome.registration = ConcernOutcome::Failed;
                log::debug!("deferred device registration failed (will back off): {err:#}");
            }
        }
    }

    Ok(outcome)
}

/// Fold a refreshed token set into the stored session + ingest bearer.
///
/// Expiry discipline: `expires_at` advances **only when a new `id_token`
/// arrived** (the id_token is the ingest bearer — advancing the clock while
/// the bearer stays stale would silently disarm future refreshes and rot
/// ingest auth). The id_token's own `exp` claim wins over `expires_in`,
/// which per RFC 6749 describes the access token, not the id_token.
fn apply_refreshed_tokens(
    config: &mut AppConfig,
    previous: &config::OidcSession,
    tokens: oidc::TokenSet,
) {
    let mut session = previous.clone();
    session.access_token = Some(tokens.access_token);
    // Honour refresh-token rotation; keep the old one if the server didn't
    // send a replacement.
    if tokens.refresh_token.is_some() {
        session.refresh_token = tokens.refresh_token;
    }

    match tokens.id_token {
        Some(id_token) => {
            let claims = oidc::decode_id_claims(&id_token).unwrap_or_default();
            session.subject = claims.sub.or(session.subject);
            session.email = claims.email.or(session.email);
            session.name = claims.name.or(session.name);
            session.expires_at = claims
                .exp
                .and_then(|secs| chrono::DateTime::<Utc>::from_timestamp(secs, 0))
                .or_else(|| {
                    tokens
                        .expires_in
                        .map(|secs| Utc::now() + chrono::Duration::seconds(secs))
                })
                .or(session.expires_at);
            session.id_token = Some(id_token.clone());
            // Only replace the ingest bearer if it still is the old id_token —
            // a pasted API key or other manual credential must never be
            // overwritten by the background loop.
            if config.server.applet_auth_token.is_none()
                || config.server.applet_auth_token == previous.id_token
            {
                config.server.applet_auth_token = Some(id_token);
            }
        }
        None => {
            // No new id_token: the bearer is still the old one, so keep the
            // old expiry — the loop will keep trying rather than silently
            // marking a stale bearer as fresh.
            log::warn!("token refresh response carried no id_token; bearer left unchanged");
        }
    }
    config.oidc_session = Some(session);
}

#[cfg(test)]
mod login_cli_tests {
    use super::*;
    use base64::Engine;

    fn jwt(payload: serde_json::Value) -> String {
        let seg = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("eyJhbGciOiJFZERTQSJ9.{seg}.sig")
    }

    #[test]
    fn decodes_string_aud_and_iss() {
        let token = jwt(serde_json::json!({
            "aud": "ci-applet",
            "iss": "https://hub.companionintelligence.com"
        }));
        let (aud, iss) = decode_jwt_aud_iss(&token).unwrap();
        assert_eq!(aud, "ci-applet");
        assert_eq!(iss, "https://hub.companionintelligence.com");
    }

    #[test]
    fn joins_array_aud() {
        let token = jwt(serde_json::json!({ "aud": ["ci-applet", "ci-server"] }));
        let (aud, _) = decode_jwt_aud_iss(&token).unwrap();
        assert_eq!(aud, "ci-applet,ci-server");
    }

    #[test]
    fn missing_claims_default_to_dash() {
        let (aud, iss) = decode_jwt_aud_iss(&jwt(serde_json::json!({}))).unwrap();
        assert_eq!(aud, "-");
        assert_eq!(iss, "-");
    }

    #[test]
    fn rejects_non_jwt() {
        assert!(decode_jwt_aud_iss("not-a-jwt").is_none());
        assert!(decode_jwt_aud_iss("").is_none());
    }
}
