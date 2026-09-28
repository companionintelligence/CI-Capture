// A binary built for the console subsystem (Rust's default) gets a console
// window allocated by the loader *before* `main` runs. For a tray app that
// means a bare black `cmd`-style window sitting on the taskbar for the whole
// session, and — because every Settings/Setup/Upload window and every
// `--check-*` probe is a re-exec of this same binary — one more flash for each
// helper. Opting into the GUI subsystem removes the console entirely; stdout
// and stderr then go nowhere, which is why this is release-only: a debug build
// keeps its console so `RUST_LOG` output stays visible while developing.
// macOS and Linux ignore the attribute.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod analysis;
mod backlog;
mod bluebubbles;
mod brand;
mod capture;
mod capture_store;
mod config;
mod connection_test;
mod conversion;
mod db;
mod error_body;
mod extractors;
mod file_tracker;
mod helper_window;
mod imessage;
mod knowledge;
mod locale;
mod lock_detect;
#[cfg(feature = "legacy-minio")]
mod minio;
mod oidc;
mod onboarding_ui;
mod permissions;
mod portal_apps;
mod queue;
mod server_check;
mod server_common;
mod server_ingest;
mod server_upload;
mod settings_ui;
mod svg_icon;
mod sync;
mod telemetry;
mod theme;
mod tokens;
mod upload_ui;
mod watcher;
mod worker;

// Loads every `locales/*.yml` at compile time and makes `t!` available crate-wide.
// `fallback` guarantees a missing Spanish key renders English rather than the raw
// message id — a half-translated build stays usable.
rust_i18n::i18n!("locales", fallback = "en");

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use config::{AppConfig, ConfigManager};
use env_logger::Env;
use parking_lot::Mutex;
use rust_i18n::t;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tinyfiledialogs::{self as dialogs, MessageBoxIcon};
use tokio::runtime::Runtime;
use tray_icon::menu::{MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{ClickType, Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;

use crate::worker::WorkerHandle;
use onboarding_ui::run_onboarding_ui;
use settings_ui::run_settings_ui;
use upload_ui::run_upload_ui;

#[derive(Debug, Clone)]
enum AppEvent {
    ReloadConfig,
    OnboardingClosed,
    UploadClosed,
    PairingFinished { success: bool, message: String },
}

/// Re-attach stdout/stderr to the terminal that launched us, on Windows GUI
/// builds only.
///
/// The `windows_subsystem = "windows"` attribute at the top of this file
/// removes the console — but `--check-server`, `--login` and the other
/// headless probes print their report to stdout, and a GUI-subsystem process
/// launched from PowerShell has none. `AttachConsole(ATTACH_PARENT_PROCESS)`
/// borrows the parent shell's console when there is one; Rust's stdio looks
/// the handle up on every write, so anything printed after this call lands in
/// that terminal. When the tray is launched from Explorer or a Run key there is
/// no parent console, the call fails, and we carry on windowless — which is
/// the whole point.
///
/// **What this does not fix: the shell does not wait for us.** A
/// GUI-subsystem exe invoked directly from an interactive `cmd` prompt, or
/// from PowerShell (interactive, a `.ps1`, or a GitHub Actions `pwsh` step),
/// is started and immediately backgrounded: the prompt returns first, the
/// `--check-*` report lands *after* it, and `%ERRORLEVEL%` /
/// `$LASTEXITCODE` are not updated from our exit status. Only the release
/// profile is affected (`cargo run -- --check-server` is a console build).
/// Scripted probes must force a wait — any of:
///
/// ```text
/// cmd /c start /wait "" companion-capture.exe --check-server
/// $p = Start-Process -FilePath .\companion-capture.exe -ArgumentList '--check-server' `
///        -Wait -NoNewWindow -PassThru; exit $p.ExitCode
/// $out = & .\companion-capture.exe --check-server      # capturing output makes pwsh wait
/// ```
///
/// (`cmd` running a `.bat`/`.cmd` file also waits natively.) Note too that
/// `--check-server` exits 0 whenever config loaded and the runtime built,
/// even if every probe printed `✗` — a smoke step has to assert on the
/// captured report, not the exit code alone. Whether stdout redirected to a
/// pipe/file survives `AttachConsole` when the parent *does* have a console
/// has not been verified on a runner; if it does not, only call
/// `AttachConsole` when `GetStdHandle(STD_OUTPUT_HANDLE)` is NULL/invalid.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    /// `ATTACH_PARENT_PROCESS` is `(DWORD)-1` in `consoleapi.h`.
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: a plain Win32 call with a scalar argument and no pointers; a
    // failure return is the expected outcome for a tray launch and is ignored.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    #[cfg(all(windows, not(debug_assertions)))]
    attach_parent_console();

    let env = Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env).init();

    // Bound to a named local on purpose: the guard flushes and disables the
    // client when dropped, so `let _ = …` would switch crash reporting off on
    // the very next line. Installed before the dispatch for the same reason as
    // the language — each window is a separate process, so a panic in the
    // Settings window is only reported if this ran in *that* process. Returns
    // None whenever any of the four gates is closed, which includes every build
    // with no DSN compiled in; see `telemetry`.
    let _sentry_guard = telemetry::init(saved_crash_reporting());

    // Before the dispatch, not after it — see the doc comment.
    apply_saved_language();

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

    if std::env::args().any(|arg| arg == "--check-bluebubbles") {
        bluebubbles::check::run()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--import-bluebubbles-once") {
        bluebubbles::import_once::run()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--check-imessage") {
        imessage::check::run()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--import-imessage-once") {
        imessage::import_once::run()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--check-knowledge") {
        knowledge::check::run()?;
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--import-knowledge-once") {
        knowledge::import_once::run()?;
        return Ok(());
    }

    // Scriptable entry points. These exist to be driven by macOS Shortcuts'
    // "Run Shell Script" action — which is what people mean when they ask for
    // Siri, since Siri runs Shortcuts by name. Cheap, and they work on every
    // platform rather than only where App Intents exist.
    // See docs/siri-test-plan.md.
    if let Some(query) = parse_flag_value("--search") {
        return open_memory_search(&query);
    }

    if std::env::args().any(|arg| arg == "--open-memory") {
        return open_memory_search("");
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
                // The success line is translated (`tray.login.signed_in`);
                // its failure counterpart has to be, too.
                eprintln!(
                    "{}",
                    t!("tray.cli.login_failed", error = format!("{err:#}"))
                );
                std::process::exit(1);
            }
        }
    }

    // Single-instance guard: an advisory OS-level file lock, held for the
    // whole process lifetime, so a second double-click (or a stray login
    // item plus a manual launch) never spins up a second tray icon, a second
    // capture worker, and a second `tao`/GTK event loop fighting the first
    // one for the same tray slot.
    //
    // This runs before `WorkerHandle::spawn` and before the event loop is
    // built, and the order matters in both directions. The worker is the
    // load-bearing half: it immediately starts the capture, file-tracker,
    // iMessage and knowledge lanes, which write the same SQLite stores and
    // cursor files the running instance owns and upload against the same
    // device identity. Spawning it first and only then discovering the lock
    // is held would give every duplicate launch a window of two live workers
    // racing that shared state — exactly what this guard exists to prevent,
    // so nothing that touches per-install state may be started above it. The
    // event loop matters for a milder reason: on Linux `tao` initializes GTK
    // there, and we would rather fail fast than initialize a GUI toolkit
    // just to tear it straight back down.
    //
    // `OpenOptions::write(true)` (not read-only) is required even though the
    // lock itself is purely advisory: Windows' `LockFileEx`, which `fd-lock`
    // calls under the hood, needs write access on the underlying handle to
    // take any lock at all, including what POSIX would treat as a read/write
    // (shared/exclusive) distinction with no such requirement. Opening
    // read-only here would work on macOS/Linux and fail silently-ish on
    // Windows, so it is write-mode everywhere for one code path across all
    // three platforms.
    let lock_path =
        config::single_instance_lock_path().context("locating the single-instance lock file")?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Nothing is ever written to this file — it exists purely to be
        // locked — so whether an existing one is kept or truncated makes no
        // behavioural difference. `false` is spelled out anyway because
        // clippy's `suspicious_open_options` wants `create` + `write` to
        // state its truncation intent explicitly rather than leave it to
        // the platform default.
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    let mut instance_lock = fd_lock::RwLock::new(lock_file);
    // Bound in `main`'s own scope, not passed anywhere else: `event_loop.run`
    // below never returns (it diverges into the OS event loop and the
    // process exits from inside it), so a binding that lives to the end of
    // `main`'s lexical scope holds the lock for the entire process lifetime
    // even though nothing ever explicitly drops it. The leading underscore
    // only silences the "unused" lint — the OS releases the flock on process
    // exit regardless of whether the compiler considers the binding "used".
    let _instance_guard = match instance_lock.try_write() {
        Ok(guard) => guard,
        Err(_) => {
            // Another instance already holds the lock — this is not an
            // error state, just a duplicate launch. Tell the user and exit
            // cleanly rather than racing the running instance for the tray.
            show_error(&t!("tray.error.already_running"));
            return Ok(());
        }
    };

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
        .with_menu_on_left_click(false)
        .with_tooltip("Companion Capture")
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
        settings_pid: None,
        onboarding_pid: None,
        upload_pid: None,
        pairing_in_progress: false,
    };

    // Idempotent, and run unconditionally on every startup rather than only
    // when the toggle flips — see `sync_autostart`'s doc comment for why a
    // moved binary or a manually-deleted login item needs this to self-heal.
    sync_autostart(initial_config.autostart.enabled);

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

/// Apply the saved UI language to this process.
///
/// Runs before `main`'s argument dispatch because *every* branch past that
/// dispatch renders catalogue strings: the tray, the three child windows —
/// each of which is its own process, with its own `rust_i18n` locale global —
/// and the scriptable `--login` / `--search` commands. Choosing the language
/// after the dispatch would leave everything except the tray speaking English.
///
/// Best-effort by design: a missing or unreadable config must not stop the app
/// from starting, and [`locale::Language::System`] — the default — is exactly
/// the right thing to fall back to when no stored choice can be read.
fn apply_saved_language() {
    let language = ConfigManager::new()
        .and_then(|manager| manager.load_or_default())
        .map(|config| config.ui_language)
        .unwrap_or_default();
    locale::apply(language);
}

/// Read the stored crash-reporting setting, which defaults to on.
///
/// Two different "no config" cases, deliberately resolved differently:
///
/// * **Absent** — a fresh install. `load_or_default` writes and returns
///   `AppConfig::default()`, where `crash_reporting` is `true`. That is the
///   opt-out posture doing its job.
/// * **Unreadable or unparseable** — this stays fail-closed and yields `false`.
///   A file we cannot read may be one in which the user switched reporting
///   *off*, and guessing wrong in that direction is the expensive mistake. The
///   cost is that the crash most worth seeing — one caused by a corrupt config —
///   is the one we decline to report; that trade is taken knowingly.
///
/// This runs before the CLI dispatch, so it is also the value used by
/// `--check-server` and the scriptable commands.
fn saved_crash_reporting() -> bool {
    ConfigManager::new()
        .and_then(|manager| manager.load_or_default())
        .map(|config| config.crash_reporting)
        .unwrap_or(false)
}

struct AppController {
    shared_config: Arc<Mutex<AppConfig>>,
    manager: ConfigManager,
    worker: WorkerHandle,
    menu_state: MenuState,
    tray_icon: Option<TrayIcon>,
    proxy: EventLoopProxy<AppEvent>,
    /// PID of the live `--settings-ui` child, if any — used to focus it when
    /// the user clicks Open Settings again instead of silently no-oping.
    settings_pid: Option<u32>,
    /// PID of the live `--onboarding-ui` child, same focus behaviour.
    onboarding_pid: Option<u32>,
    /// PID of the live `--upload-ui` child, same focus behaviour.
    upload_pid: Option<u32>,
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
        } else if menu_id == *self.menu_state.open_memory_item.id() {
            self.open_memory(&[]);
        } else if menu_id == *self.menu_state.today_item.id() {
            let from = Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc().to_rfc3339())
                .unwrap_or_default();
            let to = Utc::now().to_rfc3339();
            self.open_memory(&[("from", &from), ("to", &to), ("view", "list")]);
        } else if menu_id == *self.menu_state.settings_item.id() {
            self.open_settings_window();
        } else if menu_id == *self.menu_state.login_item.id() {
            self.launch_portal_login();
        } else if menu_id == *self.menu_state.logout_item.id() {
            self.sign_out();
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
        if tray_click_opens_settings(event.click_type) {
            self.open_settings_window();
        }
    }

    fn handle_app_event(&mut self, event: AppEvent, _target: &EventLoopWindowTarget<AppEvent>) {
        match event {
            AppEvent::ReloadConfig => {
                self.settings_pid = None;
                if let Err(err) = self.reload_config_from_disk() {
                    log::error!("failed to reload config: {err:?}");
                    show_error(&t!("tray.error.reload_config"));
                }
            }
            AppEvent::OnboardingClosed => {
                self.onboarding_pid = None;
                if let Err(err) = self.reload_config_from_disk() {
                    log::error!("failed to reload config after onboarding: {err:?}");
                }
            }
            AppEvent::UploadClosed => {
                // Upload has no config of its own to reload — nothing writes
                // to config.json from that window — so clearing the PID is
                // the whole handler, unlike Settings/Onboarding above.
                self.upload_pid = None;
            }
            AppEvent::PairingFinished { success, message } => {
                self.pairing_in_progress = false;
                if success {
                    if let Err(err) = self.reload_config_from_disk() {
                        log::error!("failed to reload config after pairing: {err:?}");
                        show_error(&t!("tray.error.reload_after_pairing"));
                        return;
                    }
                    dialogs::message_box_ok("Companion Capture", &message, MessageBoxIcon::Info);
                } else {
                    show_error(&message);
                }
            }
        }
    }

    fn open_settings_window(&mut self) {
        if let Some(pid) = self.settings_pid {
            if process_is_running(pid) {
                // Focus may report false even when the window is already
                // frontmost — only relaunch when the PID itself is gone.
                let _ = bring_pid_to_front(pid);
                return;
            }
            // Child is gone but we never got ReloadConfig — clear and relaunch.
            self.settings_pid = None;
        }
        let Ok(exe) = std::env::current_exe() else {
            show_error(&t!("tray.error.exe_settings"));
            return;
        };
        let proxy = self.proxy.clone();
        match Command::new(exe).arg("--settings-ui").spawn() {
            Ok(mut child) => {
                self.settings_pid = Some(child.id());
                thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        log::error!("settings process failed: {err:?}");
                    }
                    let _ = proxy.send_event(AppEvent::ReloadConfig);
                });
            }
            Err(err) => {
                self.settings_pid = None;
                log::error!("failed to launch settings window: {err:?}");
                show_error(&t!("tray.error.launch_settings"));
            }
        }
    }

    fn open_upload_window(&mut self) {
        if let Some(pid) = self.upload_pid {
            if process_is_running(pid) {
                // Focus may report false even when the window is already
                // frontmost — only relaunch when the PID itself is gone.
                let _ = bring_pid_to_front(pid);
                return;
            }
            // Child is gone but we never got UploadClosed — clear and relaunch.
            self.upload_pid = None;
        }
        let Ok(exe) = std::env::current_exe() else {
            show_error(&t!("tray.error.exe_upload"));
            return;
        };
        let proxy = self.proxy.clone();
        match Command::new(exe).arg("--upload-ui").spawn() {
            Ok(mut child) => {
                self.upload_pid = Some(child.id());
                thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        log::error!("upload process failed: {err:?}");
                    }
                    let _ = proxy.send_event(AppEvent::UploadClosed);
                });
            }
            Err(err) => {
                self.upload_pid = None;
                log::error!("failed to launch upload window: {err:?}");
                show_error(&t!("tray.error.launch_upload"));
            }
        }
    }

    /// Open the first-run onboarding wizard (also reachable from the tray's
    /// "Setup Assistant"). Reloads config when it closes so a fresh sign-in or
    /// completed onboarding takes effect immediately.
    fn open_onboarding_window(&mut self) {
        if let Some(pid) = self.onboarding_pid {
            if process_is_running(pid) {
                let _ = bring_pid_to_front(pid);
                return;
            }
            self.onboarding_pid = None;
        }
        let Ok(exe) = std::env::current_exe() else {
            show_error(&t!("tray.error.exe_setup"));
            return;
        };
        let proxy = self.proxy.clone();
        match Command::new(exe).arg("--onboarding-ui").spawn() {
            Ok(mut child) => {
                self.onboarding_pid = Some(child.id());
                thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        log::error!("onboarding process failed: {err:?}");
                    }
                    let _ = proxy.send_event(AppEvent::OnboardingClosed);
                });
            }
            Err(err) => {
                self.onboarding_pid = None;
                log::error!("failed to launch onboarding window: {err:?}");
                show_error(&t!("tray.error.launch_setup"));
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

    /// Clear Portal session + ingest credential. Discovered Memory choice is
    /// cleared; a manually typed Memory URL is kept.
    fn sign_out(&mut self) {
        let mut config = self.current_config();
        config.oidc_session = None;
        config.server.applet_auth_token = None;
        config.identity.user_id = None;
        config.identity.device_id = None;
        if config.server.url_source == config::ServerUrlSource::Discovered {
            config.server.portal_url.clear();
            config.server.bound_device_slug = None;
            config.server.bound_device_name = None;
            config.server.url_source = config::ServerUrlSource::Manual;
        }
        if let Err(err) = self.manager.save(&config) {
            log::error!("failed to save config after sign-out: {err:?}");
            show_error(&t!("tray.error.sign_out_save"));
            return;
        }
        *self.shared_config.lock() = config.clone();
        self.worker.update_config(config.clone());
        self.menu_state
            .refresh_labels(&config, self.worker.is_running());
        log::info!("signed out of Portal; ingest credential cleared");
    }

    fn launch_portal_login(&mut self) {
        if self.pairing_in_progress {
            show_error(&t!("tray.error.login_in_progress"));
            return;
        }

        let snapshot = self.current_config();
        let issuer = snapshot.oidc.issuer.trim().to_string();
        let client_id = snapshot.oidc.client_id.clone();
        if issuer.is_empty() {
            show_error(&t!("tray.error.oidc_issuer_missing"));
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
                        message: t!(
                            "tray.error.login_failed",
                            issuer = issuer,
                            error = err,
                            client_id = client_id
                        )
                        .to_string(),
                    }
                }
            };
            let _ = proxy.send_event(event);
        });
    }

    fn show_config_path(&self) {
        let path = self.manager.path().display().to_string();
        dialogs::message_box_ok(
            &t!("tray.dialog.config_path_title"),
            &path,
            MessageBoxIcon::Info,
        );
    }

    /// Open the Companion Memory web app in the user's browser, optionally
    /// deep-linked with query parameters.
    fn open_memory(&self, query: &[(&str, &str)]) {
        let config = self.current_config();
        let url = match server_common::memory_url(&config.server.portal_url, query) {
            Ok(url) => url,
            Err(err) => {
                show_error(&t!("tray.error.memory_address", error = format!("{err:#}")));
                return;
            }
        };
        if let Err(err) = open_url(url.as_str()) {
            show_error(&t!("tray.error.open_url", url = url, error = err));
        }
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
        // The Settings window can change the UI language. This process holds
        // its own locale global, so apply the new choice before relabelling —
        // otherwise the menu bar keeps the old language until the next
        // relaunch, which is exactly the setting the user just changed.
        locale::apply(new_config.ui_language);
        // The tray owns OS-level side effects on every reload, same as the
        // language: a Settings toggle must take effect immediately, and
        // running it unconditionally (not only when the value changed) lets
        // it self-heal a login item the user deleted by hand outside the app.
        sync_autostart(new_config.autostart.enabled);
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
    open_memory_item: MenuItem,
    today_item: MenuItem,
    interval_item: MenuItem,
    threshold_item: MenuItem,
    identity_item: MenuItem,
    server_item: MenuItem,
    login_item: MenuItem,
    logout_item: MenuItem,
    setup_item: MenuItem,
    settings_item: MenuItem,
    #[cfg(feature = "legacy-postgres")]
    postgres_item: MenuItem,
    #[cfg(feature = "legacy-minio")]
    minio_item: MenuItem,
    analysis_item: MenuItem,
    show_config_item: MenuItem,
    quit_item: MenuItem,
}

impl MenuState {
    fn new(config: &AppConfig) -> Result<Self> {
        let menu = tray_icon::menu::Menu::new();
        let status_item = MenuItem::new(
            &*t!("tray.status", state = t!("tray.state_starting")),
            false,
            None,
        );
        status_item.set_enabled(false);
        let toggle_item = MenuItem::new(&*t!("tray.pause_captures"), true, None);
        let capture_now_item = MenuItem::new(&*t!("tray.capture_now"), true, None);
        let upload_item = MenuItem::new(&*t!("tray.upload_document"), true, None);
        let open_memory_item = MenuItem::new(&*t!("tray.open_memory"), true, None);
        let today_item = MenuItem::new(&*t!("tray.today_activity"), true, None);
        let interval_item = MenuItem::new(&*t!("tray.interval"), true, None);
        let threshold_item = MenuItem::new(&*t!("tray.threshold"), true, None);
        let identity_item = MenuItem::new(&*t!("tray.identity"), true, None);
        let server_item = MenuItem::new(&*t!("tray.portal_url"), true, None);
        let login_item = MenuItem::new(&*t!("tray.login"), true, None);
        let logout_item = MenuItem::new(&*t!("tray.sign_out"), true, None);
        let setup_item = MenuItem::new(&*t!("tray.setup_assistant"), true, None);
        let settings_item = MenuItem::new(&*t!("tray.open_settings"), true, None);
        #[cfg(feature = "legacy-postgres")]
        let postgres_item = MenuItem::new(&*t!("tray.configure_postgres"), true, None);
        #[cfg(feature = "legacy-minio")]
        let minio_item = MenuItem::new(&*t!("tray.configure_minio"), true, None);
        let analysis_item = MenuItem::new(&*t!("tray.configure_vision"), true, None);
        let show_config_item = MenuItem::new(&*t!("tray.show_config"), true, None);
        let quit_item = MenuItem::new(&*t!("tray.quit"), true, None);

        interval_item.set_enabled(false);
        threshold_item.set_enabled(false);
        identity_item.set_enabled(false);
        server_item.set_enabled(false);
        #[cfg(feature = "legacy-postgres")]
        postgres_item.set_enabled(false);
        #[cfg(feature = "legacy-minio")]
        minio_item.set_enabled(false);
        analysis_item.set_enabled(false);

        menu.append_items(&[
            &status_item,
            &PredefinedMenuItem::separator(),
            &toggle_item,
            &capture_now_item,
            &upload_item,
            &PredefinedMenuItem::separator(),
            &open_memory_item,
            &today_item,
            &PredefinedMenuItem::separator(),
            &interval_item,
            &threshold_item,
            &identity_item,
            &server_item,
            &login_item,
            &logout_item,
            #[cfg(feature = "legacy-postgres")]
            &postgres_item,
            #[cfg(feature = "legacy-minio")]
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
            open_memory_item,
            today_item,
            interval_item,
            threshold_item,
            identity_item,
            server_item,
            login_item,
            logout_item,
            setup_item,
            settings_item,
            #[cfg(feature = "legacy-postgres")]
            postgres_item,
            #[cfg(feature = "legacy-minio")]
            minio_item,
            analysis_item,
            show_config_item,
            quit_item,
        };
        state.refresh_labels(config, true);
        Ok(state)
    }

    fn refresh_labels(&self, config: &AppConfig, running: bool) {
        // Truthful status: missing sign-in, missing Memory, or blocked
        // permissions must not read as "Monitoring".
        let connection = tray_connection(config);
        let backlog = crate::backlog::collect(config);
        let disconnected = !matches!(connection, TrayConnection::Ready);
        let status = if crate::backlog::should_warn(&backlog, disconnected) {
            t!("tray.state_backlog", count = backlog.waiting_count())
        } else {
            match connection {
                TrayConnection::NeedsSignIn => t!("tray.state_sign_in_needed"),
                TrayConnection::NeedsMemory => t!("tray.state_memory_disconnected"),
                TrayConnection::Ready
                    if permissions::permissions_relevant()
                        && permissions::status().needs_attention() =>
                {
                    t!("tray.state_permissions_needed")
                }
                // Signed in with a Memory URL but device registration hasn't landed
                // (often Hub edge blocked POST /devices). Don't claim Monitoring.
                TrayConnection::Ready
                    if config.oidc_session.is_some() && config.identity.device_id.is_none() =>
                {
                    t!("tray.state_device_pending")
                }
                TrayConnection::Ready if running => t!("tray.state_monitoring"),
                TrayConnection::Ready => t!("tray.state_paused"),
            }
        };
        self.status_item
            .set_text(&*t!("tray.status", state = status));
        self.toggle_item.set_text(&*if running {
            t!("tray.pause_captures")
        } else {
            t!("tray.resume_captures")
        });
        self.interval_item.set_text(&*t!(
            "tray.interval_value",
            secs = config.capture.screenshot_interval_secs
        ));
        self.threshold_item.set_text(&*t!(
            "tray.threshold_value",
            percent = format!("{:.1}", config.capture.diff_threshold_percent)
        ));
        self.identity_item.set_text(&*t!(
            "tray.identity_value",
            user = short_uuid(config.identity.user_id),
            device = short_uuid(config.identity.device_id)
        ));
        let memory_url = config.server.portal_url.trim();
        self.server_item.set_text(&*if memory_url.is_empty() {
            t!("tray.memory_url_missing")
        } else if let Some(name) = config
            .server
            .bound_device_name
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            t!("tray.memory_device_value", device = name)
        } else {
            t!("tray.memory_url_value", url = memory_url)
        });
        let signed_in = config.oidc_session.is_some() || server_ingest::is_configured(config);
        if let Some(session) = &config.oidc_session {
            let who = session
                .name
                .as_deref()
                .or(session.email.as_deref())
                .unwrap_or("Portal");
            self.login_item
                .set_text(&*t!("tray.signed_in_as", who = who));
        } else if server_ingest::is_configured(config) {
            self.login_item.set_text(&*t!("tray.signed_in_api_key"));
        } else {
            let issuer_display = config
                .oidc
                .issuer
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            self.login_item
                .set_text(&*t!("tray.login_via", issuer = issuer_display));
        }
        self.logout_item.set_text(&*t!("tray.sign_out"));
        self.logout_item.set_enabled(signed_in);
        #[cfg(feature = "legacy-postgres")]
        self.postgres_item.set_text(&*t!(
            "tray.postgres_value",
            host = config.postgres.host,
            port = config.postgres.port,
            database = config.postgres.database
        ));
        #[cfg(feature = "legacy-minio")]
        self.minio_item.set_text(&*t!(
            "tray.minio_value",
            bucket = config.minio.bucket,
            endpoint = config.minio.endpoint
        ));
        // Empty endpoint is the default and means the server describes captures;
        // naming a local model the app isn't using reads as configuration the
        // user has to attend to.
        self.analysis_item
            .set_text(&*if config.analysis.endpoint.trim().is_empty() {
                t!("tray.vision_server")
            } else {
                t!("tray.vision_local", model = config.analysis.model)
            });

        // Disabled rows keep a short reason in the label — on macOS that is
        // the only explanation visible without clicking (which does nothing).
        let ready = matches!(connection, TrayConnection::Ready);
        let has_memory = !memory_url.is_empty();
        self.capture_now_item.set_text(&*match connection {
            TrayConnection::NeedsSignIn => t!("tray.capture_now_needs_signin"),
            TrayConnection::NeedsMemory => t!("tray.capture_now_needs_memory"),
            TrayConnection::Ready => t!("tray.capture_now"),
        });
        self.upload_item.set_text(&*match connection {
            TrayConnection::NeedsSignIn => t!("tray.upload_needs_signin"),
            TrayConnection::NeedsMemory => t!("tray.upload_needs_memory"),
            TrayConnection::Ready => t!("tray.upload_document"),
        });
        self.open_memory_item.set_text(&*if has_memory {
            t!("tray.open_memory")
        } else {
            t!("tray.open_memory_needs_memory")
        });
        self.today_item.set_text(&*if has_memory {
            t!("tray.today_activity")
        } else {
            t!("tray.today_needs_memory")
        });
        self.setup_item.set_text(&*if ready {
            t!("tray.setup_assistant")
        } else {
            t!("tray.setup_assistant_connect")
        });
        self.settings_item.set_text(&*t!("tray.open_settings"));
        self.show_config_item.set_text(&*t!("tray.show_config"));
        self.quit_item.set_text(&*t!("tray.quit"));

        self.capture_now_item.set_enabled(ready);
        self.upload_item.set_enabled(ready);
        // Opening Memory only needs the server address; auth is for ingest.
        self.open_memory_item.set_enabled(has_memory);
        self.today_item.set_enabled(has_memory);
    }
}

/// What the tray needs before capture/upload can honestly say "ready".
///
/// Sign-in (`applet_auth_token`) and Companion Memory URL are independent:
/// Portal login can succeed while discovery leaves `portal_url` empty, and a
/// typed URL can sit without a credential. Surface each case separately so the
/// menu tells people the next step instead of a vague "Setup needed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayConnection {
    NeedsSignIn,
    NeedsMemory,
    Ready,
}

/// Left-click (and double-click) open Settings. Right-click keeps the menu.
fn tray_click_opens_settings(click: ClickType) -> bool {
    matches!(click, ClickType::Left | ClickType::Double)
}

fn tray_connection(config: &AppConfig) -> TrayConnection {
    let has_auth = server_ingest::is_configured(config);
    let has_memory = !config.server.portal_url.trim().is_empty();
    match (has_auth, has_memory) {
        (false, _) => TrayConnection::NeedsSignIn,
        (true, false) => TrayConnection::NeedsMemory,
        (true, true) => TrayConnection::Ready,
    }
}

fn build_icon() -> Icon {
    match svg_icon::load_tray_icon() {
        Ok(icon) => icon,
        Err(err) => {
            log::warn!("Failed to rasterize the tray glyph: {err}. Falling back to a drawn icon");
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
    dialogs::message_box_ok("Companion Capture", message, MessageBoxIcon::Error);
}

/// Open a URL in the user's default browser, cross-platform.
///
/// Deep links carry query strings, which is what makes the Windows arm subtle:
/// True when `pid` still refers to a live process we can signal.
fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SIGNAL 0 — existence / permission check, does not deliver a signal.
        // SAFETY: kill(pid, 0) is the standard POSIX liveness probe.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        // No cheap liveness check wired here; trust the pid until the child
        // wait thread clears it via ReloadConfig / OnboardingClosed.
        let _ = pid;
        true
    }
}

/// Raise an already-running helper window (Settings / Setup) owned by `pid`.
///
/// Settings runs as a separate process (`--settings-ui`). A second tray click
/// used to no-op while that process lived, leaving the window buried behind
/// other apps. Activating by PID brings that exact process forward (the tray
/// and Settings share a bundle id, so activating "the app" would be ambiguous).
fn bring_pid_to_front(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc::{class, msg_send, sel, sel_impl};

        // NSApplicationActivateIgnoringOtherApps
        const ACTIVATE_IGNORING_OTHER_APPS: u64 = 1 << 1;

        // SAFETY: AppKit is linked via tray-icon/tao. Null means the PID is gone.
        unsafe {
            let app: *mut objc::runtime::Object = msg_send![
                class!(NSRunningApplication),
                runningApplicationWithProcessIdentifier: pid as i32
            ];
            if app.is_null() {
                return false;
            }
            let ok: bool = msg_send![app, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
            ok
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::{FALSE, HWND, LPARAM, TRUE};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SW_RESTORE,
            SetForegroundWindow, ShowWindow,
        };
        use windows_sys::core::BOOL;

        // Carries the search target in and the result out of `enum_proc`.
        // `EnumWindows` has no typed way to thread state through its C
        // callback other than the single `LPARAM` it forwards verbatim, so a
        // pointer to this struct is round-tripped through it: cast to
        // `LPARAM` going in below, cast back going out inside `enum_proc`.
        struct SearchState {
            target_pid: u32,
            found: Option<HWND>,
        }

        // SAFETY (of the `extern "system" fn` itself): `EnumWindows` invokes
        // this callback synchronously, once per top-level window, entirely
        // within the single `EnumWindows` call below — it never runs after
        // that call returns and never runs concurrently on another thread.
        // That is what makes the `lparam -> &mut SearchState` pointer
        // round-trip sound: the `SearchState` it points at lives on this
        // function's stack for the whole `EnumWindows` call, so the pointer
        // never outlives what it points to.
        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: `lparam` is exactly the `&mut SearchState as LPARAM`
            // passed to `EnumWindows` a few lines below, and per the
            // reasoning above that reference is still live.
            let state = unsafe { &mut *(lparam as *mut SearchState) };
            let mut process_id: u32 = 0;
            // SAFETY: `hwnd` is a valid top-level window handle supplied by
            // `EnumWindows` itself; this only writes through the out-pointer
            // we give it.
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut process_id);
            }
            // `EnumWindows` walks *every* top-level window, and an egui/eframe
            // process owns more than the one the user can see: hidden helper
            // and message-only windows belong to the same PID. Matching on the
            // PID alone would happily stop at one of those, and
            // `SetForegroundWindow` on an invisible window is a silent no-op —
            // the real Settings window would stay buried, which is the exact
            // bug this helper exists to fix. Requiring `IsWindowVisible` keeps
            // the enumeration walking until it reaches a window that can
            // actually come forward.
            //
            // SAFETY: same live `hwnd` from `EnumWindows` as above; this only
            // reads the window's visibility style.
            if process_id == state.target_pid && unsafe { IsWindowVisible(hwnd) } != FALSE {
                state.found = Some(hwnd);
                FALSE // match found — stop enumerating
            } else {
                TRUE // keep looking
            }
        }

        let mut state = SearchState {
            target_pid: pid,
            found: None,
        };
        // SAFETY: `enum_proc` matches `WNDENUMPROC`'s required signature and
        // calling convention, and `&mut state` outlives this call (it is a
        // local that is not dropped until after `EnumWindows` returns).
        unsafe {
            EnumWindows(Some(enum_proc), &mut state as *mut SearchState as LPARAM);
        }
        match state.found {
            Some(hwnd) => {
                // SAFETY: `hwnd` was handed to us by `GetWindowThreadProcessId`
                // via a live `EnumWindows` enumeration moments ago, so it
                // still names a real window handle.
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd) != 0
                }
            }
            None => false,
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = pid;
        // No portable, PID-keyed "raise that window" primitive exists across
        // Linux's fragmented windowing stack: X11/EWMH (`_NET_ACTIVE_WINDOW`
        // via a root-window client message), GNOME Shell (its own
        // `org.gnome.Shell` D-Bus Eval/extension surface), and wlroots
        // Wayland compositors (no activation protocol keyed by PID at all,
        // by design — Wayland deliberately withholds cross-client window
        // control) each need an entirely different mechanism. Picking one
        // would add a windowing dependency that is dead code on whichever of
        // those the user is *not* running, for a feature (raising an
        // already-open Settings window) that degrades gracefully today: the
        // caller just avoids spawning a duplicate while the PID is alive. A
        // reliable Linux implementation is left to CI-Engineering #284's
        // follow-up.
        false
    }
}

/// Build the `AutoLaunch` handle for this executable.
///
/// `MacOSLaunchMode::LaunchAgent` is chosen over the newer `SMAppService`
/// deliberately: `SMAppService` requires the binary to live inside a real,
/// macOS-13+-registered `.app` bundle at `mainApp` scope, which a bare
/// `cargo run` dev binary is not — every local dev loop and every CI check
/// that exercises this code would need a full bundle-and-codesign step just
/// to reach the API. A `LaunchAgent` (a plist dropped in
/// `~/Library/LaunchAgents/`) works identically whether the target is a dev
/// binary or a packaged release, so one code path covers both, and testing
/// this doesn't require a signed bundle.
fn build_auto_launch(app_path: &str) -> auto_launch::Result<auto_launch::AutoLaunch> {
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name(config::APP_NAME)
        .set_app_path(app_path)
        .set_args(&[] as &[&str])
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent)
        .set_linux_launch_mode(auto_launch::LinuxLaunchMode::XdgAutostart)
        .set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser)
        .build()
}

/// Reconcile the OS-level login item with `enabled`.
///
/// Called unconditionally on every relevant config load — at tray startup
/// and after every Settings reload — not only when the toggle actually
/// flips. That makes it idempotent by construction and lets it self-heal: if
/// the binary moved (an app update replacing the executable at a new path)
/// or the user manually deleted the LaunchAgent plist / registry Run key /
/// XDG autostart `.desktop` file outside the app, the next reload silently
/// restores the login item to match the saved preference instead of leaving
/// it stale or gone. Same philosophy as `imessage`'s legacy-cursor-discard
/// reasoning: derive the OS-visible state from the config every time rather
/// than trusting a one-shot mutation to have stuck.
///
/// Best-effort: a failure here is never fatal to the tray starting or a
/// Settings save completing, since the applet's core job (capturing and
/// syncing) is unaffected either way. Every failure is logged so it's
/// diagnosable, never surfaced as a blocking dialog for what is a background
/// convenience setting.
fn sync_autostart(enabled: bool) {
    let Ok(exe) = std::env::current_exe() else {
        log::warn!("autostart: could not resolve the running executable path");
        return;
    };
    let Some(exe) = exe.to_str() else {
        log::warn!("autostart: executable path is not valid UTF-8");
        return;
    };
    match build_auto_launch(exe) {
        Ok(auto) => {
            let result = if enabled {
                auto.enable()
            } else {
                auto.disable()
            };
            if let Err(err) = result {
                log::warn!(
                    "autostart: failed to {} ({err:?})",
                    if enabled { "enable" } else { "disable" }
                );
            }
        }
        Err(err) => log::warn!("autostart: could not build the launcher ({err:?})"),
    }
}

/// `cmd.exe` re-parses the raw command line and splits on `&` *before* argument
/// parsing, so a plain `Command::arg` — whose MSVC-style quoting `cmd.exe` does
/// not honour — truncates every multi-parameter URL at the first `&`. Passing
/// the pieces through `raw_arg` and quoting the URL ourselves is what actually
/// suppresses `& | ^ < >`.
fn open_url(url: &str) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        use std::os::windows::process::CommandExt;

        /// `CREATE_NO_WINDOW` from `processthreadsapi.h`. `cmd.exe` is a
        /// console program, so without this flag every "Open Memory" or
        /// "Open Portal" click flashes a black console for the instant it
        /// takes `start` to hand the URL to the default browser. Spelled out
        /// rather than pulled from `windows-sys` to keep the direct
        /// dependency list unchanged; the value is ABI-stable.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        // A literal `"` would close our quoting and let the rest of the string
        // reach the shell unquoted. Valid URLs percent-encode it as %22, so
        // dropping it is both safe and the conservative choice.
        let sanitized = url.replace('"', "");
        let mut c = Command::new("cmd");
        c.creation_flags(CREATE_NO_WINDOW);
        c.raw_arg("/C");
        c.raw_arg("start");
        // `start` reads a leading quoted token as the window title; give it an
        // empty one so it treats ours as the target.
        c.raw_arg("\"\"");
        c.raw_arg(format!("\"{sanitized}\""));
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

/// Open Companion Memory in the browser, optionally deep-linked to a search.
///
/// Runs in the *invoking* process rather than messaging the tray, so it works
/// whether or not the menu-bar app is running — a Shortcut should not fail
/// because the user quit the tray.
fn open_memory_search(query: &str) -> Result<()> {
    let manager = ConfigManager::new().context("loading config")?;
    let config = manager.load_or_default().context("loading config")?;

    let query = query.trim();
    let params: Vec<(&str, &str)> = if query.is_empty() {
        Vec::new()
    } else {
        vec![("search", query), ("view", "list")]
    };

    // These two reach the user: `main` prints the returned error, and a
    // Shortcut surfaces it verbatim. `--login` is a CLI flag, not prose, so it
    // stays literal inside the sentence.
    if config.server.portal_url.trim().is_empty() {
        bail!("{}", t!("tray.cli.no_server"));
    }
    let url = server_common::memory_url(&config.server.portal_url, &params).map_err(|_| {
        anyhow!(
            "{}",
            t!(
                "tray.cli.bad_server_url",
                url = config.server.portal_url.trim()
            )
        )
    })?;

    open_url(url.as_str()).with_context(|| format!("opening {url}"))?;
    println!("{url}");
    Ok(())
}

/// Read `--flag <value>` or `--flag=<value>` from the command line.
fn parse_flag_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let eq = format!("{flag}=");
    for (i, arg) in args.iter().enumerate() {
        if let Some(rest) = arg.strip_prefix(&eq) {
            return Some(rest.to_string());
        }
        if arg == flag {
            return args.get(i + 1).cloned();
        }
    }
    None
}

/// Parse an optional `--portal <url>` / `--portal=<url>` override used by
/// `--check-server` to probe an arbitrary endpoint without mutating config.
/// The `--portal` override, canonicalized exactly as the settings form and the
/// setup assistant canonicalize a typed address.
///
/// Without this, `--check-server --portal core-1:8642` reports
/// "portal URL is missing a hostname" for an address the GUI accepts — so the
/// self-test disagrees with the app about what a valid address is, which is
/// precisely when someone reaches for the self-test. An address it can't make
/// sense of is passed through untouched so the report shows what was typed.
fn parse_portal_override() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let raw = args.iter().enumerate().find_map(|(i, arg)| {
        if let Some(rest) = arg.strip_prefix("--portal=") {
            return Some(rest.to_string());
        }
        if arg == "--portal" {
            return args.get(i + 1).cloned();
        }
        None
    })?;
    Some(server_common::normalize_server_url(&raw).unwrap_or(raw))
}

fn short_uuid(id: Option<Uuid>) -> String {
    match id {
        Some(uuid) => uuid
            .to_string()
            .split('-')
            .next()
            .unwrap_or("?")
            .to_string(),
        None => t!("tray.identity_unset").to_string(),
    }
}

/// Sign in to the portal via OIDC (Authorization Code + PKCE), persist the
/// resulting tokens + identity, and best-effort register this device.
/// First launch = no server credential configured and onboarding never
/// finished. Drives whether the setup assistant auto-opens.
fn is_first_run(config: &AppConfig) -> bool {
    !config.onboarding_completed && !server_ingest::is_configured(config)
}

pub(crate) fn login_with_portal() -> Result<String> {
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

    // Ask the Portal where this user's Companion Memory runs, so a fresh install
    // never has to be told. Strictly best-effort: sign-in has already succeeded
    // and must not be reported as failed because discovery didn't work.
    let discovery_note = resolve_core_url(&runtime, &mut config, &outcome.tokens.access_token);

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

    // Best-effort: trade the ~1 h id_token for a durable api key while it's
    // fresh, so ingest survives past the token's expiry (the portal's refresh
    // grant never reissues an id_token). On failure the id_token stays the
    // bearer and the refresh loop retries the mint while it can.
    if !config.server.portal_url.trim().is_empty()
        && let Some(token) = id_token.as_deref()
    {
        match runtime.block_on(async { mint_api_key(&config, token).await }) {
            Ok(key) => {
                config.server.applet_auth_token = Some(key);
                log::info!("minted durable CI-Server api key for ingest");
            }
            Err(err) => {
                log::warn!("api-key mint deferred (ingest stays on id_token): {err:#}");
            }
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
        .unwrap_or_else(|| t!("tray.login.your_account").to_string());
    Ok(t!("tray.login.signed_in", who = who, note = discovery_note).to_string())
}

/// Resolve the Companion Memory server address from CI-Portal after sign-in.
///
/// Returns a sentence for the user. Never returns an error: this runs after a
/// successful sign-in, and a discovery failure means "you'll have to type the
/// address", not "signing in failed".
///
/// The credential is the **opaque access_token**, not the id_token — see
/// [`crate::portal_apps`]. Many Capture installs may share one Memory; when the
/// account has several Memories, reuse `bound_device_slug` or ask in Settings.
fn resolve_core_url(runtime: &Runtime, config: &mut AppConfig, access_token: &str) -> String {
    use portal_apps::{Discovery, MemorySelection};

    // Never move a server the user chose themselves.
    if config.server.url_source == config::ServerUrlSource::Manual
        && !config.server.portal_url.trim().is_empty()
    {
        return t!("tray.discovery.attributed").into_owned();
    }

    let portal = config
        .oidc
        .issuer
        .trim()
        .trim_end_matches("/api/auth")
        .to_string();
    let found = runtime.block_on(async {
        portal_apps::fetch_portal_apps(&portal, access_token, portal_apps::MEMORY_APP_SLUG).await
    });

    match found {
        Discovery::Ok(apps) => {
            let candidates = portal_apps::memory_candidates(&apps);
            let prior = config.server.bound_device_slug.clone();
            match portal_apps::select_memory(&candidates, prior.as_deref()) {
                MemorySelection::None => t!("tray.discovery.none").into_owned(),
                MemorySelection::Chosen(core) => {
                    portal_apps::apply_discovered_memory(config, &core);
                    log::info!(
                        "discovered Companion Memory at {} (device={:?})",
                        core.url,
                        core.device_slug
                    );
                    match &core.device_name {
                        Some(name) => t!("tray.discovery.saving_to", device = name).into_owned(),
                        None => t!("tray.discovery.connected", url = core.url).into_owned(),
                    }
                }
                MemorySelection::NeedsChoice(list) => {
                    // Leave URL empty until Settings picks — do not guess.
                    log::info!(
                        "Companion Memory discovery found {} devices; user must choose",
                        list.len()
                    );
                    t!("tray.discovery.multiple", count = list.len()).into_owned()
                }
            }
        }
        // A Portal that predates /api/users/me/apps. Say nothing about it; the
        // user simply types the address as before.
        Discovery::Unsupported => {
            log::info!("portal has no app-discovery endpoint; server address stays manual");
            t!("tray.discovery.attributed").into_owned()
        }
        Discovery::Unauthorized => {
            // Almost always means this client is not registered in the Portal's
            // PUBLIC_CLIENTS, which is an operator problem, not a user one.
            log::warn!("portal refused the access token for app discovery");
            t!("tray.discovery.unauthorized").into_owned()
        }
        Discovery::Error(err) => {
            log::warn!("Companion Memory discovery failed: {err}");
            t!("tray.discovery.error").into_owned()
        }
    }
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
        println!(
            "  subject   : {}",
            session.subject.as_deref().unwrap_or("-")
        );
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
                    println!(
                        "  token aud : {aud}   (CI-Server must list this in PORTAL_OIDC_AUDIENCE)"
                    );
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

/// Register this machine with CI-Server (`POST /devices`). `token` is the
/// Portal id_token at sign-in (sent as Bearer) or, on the deferred retry, the
/// minted API key — which must go as `x-api-key`: a CI-Hub edge only lets an
/// API key through to Memory in that header.
async fn register_device(config: &AppConfig, token: &str) -> Result<Uuid> {
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
        .no_proxy()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building device-registration client")?;

    let mut last_error = None;
    for url in server_common::devices_urls(&config.server.portal_url)? {
        match server_common::AuthCredential::from_token(token)
            .apply(client.post(url.clone()))
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

/// The stable, human-recognizable name for the api key this install mints for
/// itself. Stable so re-sign-ins rotate one key instead of accumulating rows;
/// the hostname makes it identifiable in Memory's settings UI.
fn applet_api_key_name() -> String {
    let hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string());
    format!("Companion Capture ({hostname})")
}

/// Exchange a fresh portal id_token for a durable CI-Server api key.
///
/// The id_token CI-Server accepts as an ingest bearer expires after about an
/// hour, and the portal's refresh grant never reissues it — ingest built on
/// the raw id_token rots mid-session. A key minted here carries no
/// `expires_at`, so it never expires; instead of refreshing it we rotate it
/// on every sign-in (delete-by-name, then create — CI-Server enforces a
/// unique (user, name) index). CI-Server accepts the portal bearer on
/// `/api/keys` but rejects api-key auth there, so this can only run while
/// the id_token is still fresh. Older CI-Server builds reject the portal
/// bearer on these routes entirely; the caller keeps the id_token as the
/// bearer when that happens.
async fn mint_api_key(config: &AppConfig, bearer: &str) -> Result<String> {
    let name = applet_api_key_name();
    let client = reqwest::Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building api-key client")?;

    let mut last_error = None;
    for url in server_common::keys_urls(&config.server.portal_url)? {
        // Rotate: find and delete the key a previous sign-in minted under our
        // name, or the create below trips the unique index.
        let existing = match client.get(url.clone()).bearer_auth(bearer).send().await {
            Ok(resp) if resp.status().is_success() => {
                let keys: serde_json::Value = resp.json().await.context("decoding api-key list")?;
                keys.as_array().and_then(|keys| {
                    keys.iter()
                        .find(|k| k.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
                        .and_then(|k| k.get("id").and_then(|i| i.as_str()))
                        .map(str::to_string)
                })
            }
            Ok(resp)
                if matches!(
                    resp.status(),
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
                ) =>
            {
                last_error = Some(anyhow!("GET {url} → HTTP {}", resp.status()));
                continue;
            }
            Ok(resp) => return Err(anyhow!("GET {url} → HTTP {}", resp.status())),
            Err(err) => return Err(anyhow!("GET {url}: {err}")),
        };

        if let Some(id) = existing {
            let delete_url = format!("{}/{}", url.as_str().trim_end_matches('/'), id);
            let resp = client
                .delete(&delete_url)
                .bearer_auth(bearer)
                .send()
                .await
                .map_err(|err| anyhow!("DELETE {delete_url}: {err}"))?;
            // 404 = already gone (a racing rotation won) — the name is free
            // either way.
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
                return Err(anyhow!("DELETE {delete_url} → HTTP {}", resp.status()));
            }
        }

        let resp = client
            .post(url.clone())
            .bearer_auth(bearer)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(|err| anyhow!("POST {url}: {err}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("POST {url} → HTTP {}", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.context("decoding api-key response")?;
        return v
            .get("key")
            .and_then(|k| k.as_str())
            .map(str::to_string)
            .context("api-key response missing raw key");
    }

    Err(last_error.unwrap_or_else(|| anyhow!("api-key mint failed")))
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
        let mut mint_failures: u32 = 0;
        let mut skip_refresh_ticks: u32 = 0;
        let mut skip_registration_ticks: u32 = 0;
        let mut skip_mint_ticks: u32 = 0;
        loop {
            match token_refresh_tick(
                &manager,
                &worker,
                &shared,
                &runtime,
                skew,
                skip_refresh_ticks == 0,
                skip_registration_ticks == 0,
                skip_mint_ticks == 0,
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
                            skip_registration_ticks = failure_backoff_ticks(registration_failures);
                        }
                        ConcernOutcome::Idle => {}
                    }
                    match outcome.mint {
                        ConcernOutcome::Succeeded => mint_failures = 0,
                        ConcernOutcome::Failed => {
                            mint_failures += 1;
                            skip_mint_ticks = failure_backoff_ticks(mint_failures);
                        }
                        ConcernOutcome::Idle => {}
                    }
                }
                Err(err) => log::warn!("token-refresh tick failed: {err:?}"),
            }
            thread::sleep(TOKEN_REFRESH_CHECK);
            skip_refresh_ticks = skip_refresh_ticks.saturating_sub(1);
            skip_registration_ticks = skip_registration_ticks.saturating_sub(1);
            skip_mint_ticks = skip_mint_ticks.saturating_sub(1);
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
    mint: ConcernOutcome,
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
    try_mint: bool,
) -> Result<TickOutcome> {
    let mut outcome = TickOutcome {
        refresh: ConcernOutcome::Idle,
        registration: ConcernOutcome::Idle,
        mint: ConcernOutcome::Idle,
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
                .no_proxy()
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
                    log::info!("discarding refreshed token: session was superseded during refresh");
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

    // 3) Upgrade a raw id_token ingest bearer to a durable api key. Covers a
    //    sign-in that couldn't mint (Memory URL unknown or unreachable at the
    //    time): the exchange only works while the id_token is still fresh, so
    //    retry from here until it expires. Pasted api keys and already-minted
    //    keys never equal the session id_token, so they're left alone.
    let config = manager.load_or_default()?;
    if try_mint
        && let Some(session) = config.oidc_session.clone()
        && let Some(id_token) = session.id_token.clone()
        && config.server.applet_auth_token.as_deref() == Some(id_token.as_str())
        && !config.server.portal_url.trim().is_empty()
        && !oidc::token_expiring(session.expires_at, Utc::now(), skew)
    {
        match runtime.block_on(async { mint_api_key(&config, &id_token).await }) {
            Ok(key) => {
                // Reload → merge → save; only swap the bearer if it is still
                // the id_token we exchanged.
                let mut fresh = manager.load_or_default()?;
                if fresh.server.applet_auth_token.as_deref() == Some(id_token.as_str()) {
                    fresh.server.applet_auth_token = Some(key);
                    manager.save(&fresh).context("persisting minted api key")?;
                    *shared.lock() = fresh.clone();
                    worker.update_config(fresh);
                }
                outcome.mint = ConcernOutcome::Succeeded;
                log::info!("minted durable CI-Server api key for ingest (deferred)");
            }
            Err(err) => {
                outcome.mint = ConcernOutcome::Failed;
                log::debug!("deferred api-key mint failed (will back off): {err:#}");
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
mod tray_click_tests {
    use super::*;

    #[test]
    fn left_and_double_open_settings_right_does_not() {
        assert!(tray_click_opens_settings(ClickType::Left));
        assert!(tray_click_opens_settings(ClickType::Double));
        assert!(!tray_click_opens_settings(ClickType::Right));
    }
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
