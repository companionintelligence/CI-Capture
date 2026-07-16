//! First-run onboarding wizard (`--onboarding-ui`).
//!
//! A four-step child-process window (Welcome → Connect → Permissions → Finish)
//! that gets a brand-new user from install to actually-capturing:
//!
//! - **Connect** — one-click browser sign-in (OIDC) *or* paste an API key, with
//!   a live "Test connection" against CI-Server.
//! - **Permissions** — on macOS, live Screen Recording + Accessibility status
//!   with one-click *Grant* / *Open Settings* / *Re-check* aids (the grants the
//!   capture lanes silently need). Auto-skipped where the OS doesn't gate them.
//!
//! Runs as a separate process like the settings window, and sets
//! `onboarding_completed` on finish so it doesn't reappear.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::NativeOptions;
use egui::Color32;
use parking_lot::Mutex;

use crate::config::{AppConfig, ConfigManager};
use crate::server_common::AuthCredential;
use crate::{connection_test, permissions};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Connect,
    Permissions,
    Finish,
}

/// Result of an async job (login or connection test): Ok(detail) / Err(msg).
type JobResult = Result<String, String>;

pub fn run_onboarding_ui() -> Result<()> {
    let manager = Arc::new(ConfigManager::new().context("building config manager")?);
    let config = manager.load_or_default().context("loading configuration")?;
    let completed = Arc::new(Mutex::new(false));

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Welcome to CI-Applet")
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([640.0, 520.0]),
        ..Default::default()
    };

    let manager_c = manager.clone();
    let completed_c = completed.clone();
    eframe::run_native(
        "CI-Applet Setup",
        native_options,
        Box::new(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            crate::theme::apply_visuals(&cc.egui_ctx);
            let logo = crate::theme::load_logo(&cc.egui_ctx);
            Ok(Box::new(OnboardingApp::new(
                manager_c.clone(),
                config.clone(),
                completed_c.clone(),
                Some(logo),
            )))
        }),
    )
    .map_err(|err| anyhow!("failed to run onboarding UI: {err}"))?;

    Ok(())
}

struct OnboardingApp {
    manager: Arc<ConfigManager>,
    config: AppConfig,
    completed: Arc<Mutex<bool>>,
    logo: Option<egui::TextureHandle>,
    step: Step,

    // Connect step
    server_url: String,
    oidc_issuer: String,
    api_key: String,
    login_rx: Option<Receiver<JobResult>>,
    test_rx: Option<Receiver<JobResult>>,
    status_line: Option<Result<String, String>>,

    // Permissions step
    perms: permissions::Permissions,
}

/// Prefilled portal sign-in URL when the config's issuer is empty (the config
/// default is intentionally blank/per-deployment; the wizard offers a sensible
/// starting point the user can edit).
const DEFAULT_ISSUER: &str = "https://hub.companionintelligence.com/api/auth";

impl OnboardingApp {
    fn new(
        manager: Arc<ConfigManager>,
        config: AppConfig,
        completed: Arc<Mutex<bool>>,
        logo: Option<egui::TextureHandle>,
    ) -> Self {
        let oidc_issuer = if config.oidc.issuer.trim().is_empty() {
            DEFAULT_ISSUER.to_string()
        } else {
            config.oidc.issuer.clone()
        };
        Self {
            server_url: config.server.portal_url.clone(),
            oidc_issuer,
            api_key: String::new(),
            login_rx: None,
            test_rx: None,
            status_line: None,
            perms: permissions::status(),
            manager,
            config,
            completed,
            logo,
            step: Step::Welcome,
        }
    }

    fn has_credential(&self) -> bool {
        crate::server_ingest::is_configured(&self.config)
    }

    /// Persist a wizard-owned change by merging it onto the **live on-disk**
    /// config, not over the startup snapshot. The main process's token-refresh
    /// loop may have rotated `oidc_session` / `applet_auth_token` / device id
    /// while this window was open (it's reachable any time from the tray), and
    /// a whole-snapshot write would revert that — silently disarming refresh.
    /// Mirrors settings_ui/build_config and login_with_portal's reload-merge.
    fn persist_merged(&mut self, mutate: impl FnOnce(&mut AppConfig)) {
        let mut live = self
            .manager
            .load_or_default()
            .unwrap_or_else(|_| self.config.clone());
        mutate(&mut live);
        match self.manager.save(&live) {
            Ok(()) => self.config = live,
            Err(e) => self.status_line = Some(Err(format!("Could not save: {e}"))),
        }
    }

    /// Persist the current CI-Server URL + OIDC issuer (merge-safe).
    fn save_connection_fields(&mut self) {
        let url = self.server_url.trim().to_string();
        let issuer = self.oidc_issuer.trim().to_string();
        self.persist_merged(|c| {
            c.server.portal_url = url;
            c.oidc.issuer = issuer;
        });
    }

    fn reload_config(&mut self) {
        if let Ok(fresh) = self.manager.load_or_default() {
            self.config = fresh;
        }
    }

    // ── async jobs ──────────────────────────────────────────────────────────

    fn start_login(&mut self) {
        if self.oidc_issuer.trim().is_empty() {
            self.status_line = Some(Err(
                "Enter your portal sign-in URL (OIDC issuer) first, or paste an API key below."
                    .into(),
            ));
            return;
        }
        self.save_connection_fields();
        let (tx, rx) = channel();
        self.login_rx = Some(rx);
        self.status_line = Some(Ok("Opening your browser to sign in…".into()));
        std::thread::spawn(move || {
            // login_with_portal opens the browser, waits for the loopback
            // redirect, and persists the tokens to config.json.
            let result = crate::login_with_portal().map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    fn start_api_key_save(&mut self) {
        let key = self.api_key.trim().to_string();
        if key.is_empty() {
            self.status_line = Some(Err("Paste an API key first.".into()));
            return;
        }
        let url = self.server_url.trim().to_string();
        // Choosing an API key means API-key auth: store the key and clear any
        // stale OIDC session so the refresh loop doesn't fight it.
        self.persist_merged(|c| {
            c.server.portal_url = url;
            c.server.applet_auth_token = Some(key);
            c.oidc_session = None;
        });
        if self.has_credential() {
            self.status_line = Some(Ok("API key saved. Testing the connection…".into()));
            self.start_connection_test();
        }
    }

    fn start_connection_test(&mut self) {
        self.save_connection_fields();
        let (tx, rx) = channel();
        self.test_rx = Some(rx);
        let config = self.config.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("runtime: {e}")));
                    return;
                }
            };
            let result = runtime.block_on(connection_test::test_ci_server(&config));
            let _ = tx.send(result);
        });
    }

    /// Drain any finished async job into `status_line`.
    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.login_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.login_rx = None;
                    if result.is_ok() {
                        self.reload_config();
                    }
                    self.status_line = Some(result);
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.login_rx = None,
            }
        }
        if let Some(rx) = &self.test_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.test_rx = None;
                    self.status_line = Some(result);
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.test_rx = None,
            }
        }
    }

    fn job_running(&self) -> bool {
        self.login_rx.is_some() || self.test_rx.is_some()
    }

    fn finish(&mut self, ctx: &egui::Context) {
        self.reload_config();
        self.config.onboarding_completed = true;
        let _ = self.manager.save(&self.config);
        *self.completed.lock() = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ── rendering ─────────────────────────────────────────────────────────────

    fn header(&self, ui: &mut egui::Ui, title: &str) {
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.image((logo.id(), egui::vec2(36.0, 36.0)));
            }
            ui.heading(title);
        });
        // Step dots.
        let steps = [Step::Welcome, Step::Connect, Step::Permissions, Step::Finish];
        ui.horizontal(|ui| {
            for s in steps {
                let on = s == self.step;
                ui.label(egui::RichText::new(if on { "●" } else { "○" }).color(if on {
                    Color32::from_rgb(0x5c, 0x8d, 0xff)
                } else {
                    Color32::GRAY
                }));
            }
        });
        ui.separator();
    }

    fn status_widget(&self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status_line {
            match status {
                Ok(msg) => {
                    ui.colored_label(Color32::from_rgb(0x2d, 0xd4, 0xb0), format!("✓ {msg}"));
                }
                Err(msg) => {
                    ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), format!("✗ {msg}"));
                }
            }
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label("CI-Applet grows your Companion Memory by capturing your desktop and streaming it to your own CI-Server.");
        ui.add_space(8.0);
        ui.label(egui::RichText::new("It captures three things:").strong());
        ui.label("📸  Screenshots of your screen, on an interval (diff-gated).");
        ui.label("🧭  System activity — foreground app, window, working directory.");
        ui.label("📄  Edits to folders you choose — so your AI remembers your files.");
        ui.add_space(10.0);
        ui.label(egui::RichText::new("Private by design:").strong());
        ui.label("• Local-first — capture streams only to the CI-Server you sign in to.");
        ui.label("• File tracking is off until you add a folder yourself.");
        ui.label("• Credential stores (.ssh, .env, keychains) are never sent.");
        ui.add_space(16.0);
        egui::TopBottomPanel::bottom("welcome_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Get started  →").clicked() {
                    self.step = Step::Connect;
                }
            });
        });
    }

    fn connect(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label("Point the applet at your CI-Server and sign in.");
        ui.add_space(8.0);

        ui.label(egui::RichText::new("CI-Server URL").strong());
        ui.add(
            egui::TextEdit::singleline(&mut self.server_url)
                .hint_text("https://your-ci-server or http://core-1:8642")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(12.0);

        let running = self.job_running();
        ui.add_enabled_ui(!running, |ui| {
            ui.label(egui::RichText::new("Portal sign-in URL").strong());
            ui.add(
                egui::TextEdit::singleline(&mut self.oidc_issuer)
                    .hint_text("https://your-portal/api/auth")
                    .desired_width(f32::INFINITY),
            );
            ui.horizontal(|ui| {
                if ui.button("🔓  Sign in with browser").clicked() {
                    self.start_login();
                }
                ui.label("— opens your portal login (recommended).");
            });
            ui.add_space(10.0);
            ui.label(egui::RichText::new("…or paste an API key").strong());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key)
                        .password(true)
                        .hint_text("CI-Server API key or bearer token")
                        .desired_width(320.0),
                );
                if ui.button("Save key").clicked() {
                    self.start_api_key_save();
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running && self.has_credential(), egui::Button::new("Test connection"))
                .clicked()
            {
                self.start_connection_test();
            }
            if running {
                ui.spinner();
            }
            let cred = if self.has_credential() {
                match AuthCredential::from_config(&self.config).ok() {
                    Some(c) => format!("credential stored ({})", c.label()),
                    None => "credential stored".into(),
                }
            } else {
                "no credential yet".into()
            };
            ui.label(egui::RichText::new(cred).weak());
        });
        ui.add_space(6.0);
        self.status_widget(ui);

        egui::TopBottomPanel::bottom("connect_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("←  Back").clicked() {
                    self.step = Step::Welcome;
                }
                let next = if permissions::permissions_relevant() {
                    Step::Permissions
                } else {
                    Step::Finish
                };
                // Always offer exactly one forward control so no one gets
                // stranded (and the wizard can always reach Finish, which sets
                // onboarding_completed and stops the auto-reopen).
                if self.has_credential() {
                    if ui.button("Next  →").clicked() {
                        self.save_connection_fields();
                        self.perms = permissions::status();
                        self.step = next;
                    }
                } else {
                    if ui.button("Skip for now  →").clicked() {
                        self.step = next;
                    }
                    ui.label(
                        egui::RichText::new("You can sign in later from the tray.").weak(),
                    );
                }
            });
        });
    }

    fn permissions(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label("macOS needs two permissions for the applet to actually see your screen and activity. Without them, capture silently produces blank frames.");
        ui.add_space(10.0);

        let sr = self.perms.screen_recording;
        let ax = self.perms.accessibility;

        permission_row(
            ui,
            "Screen Recording",
            "Required for screenshots.",
            sr,
            |ui| {
                if sr == permissions::PermissionStatus::Denied
                    && ui.button("Grant…").clicked()
                {
                    // Triggers the OS prompt on first call.
                    permissions::request_screen_recording();
                }
                if ui.button("Open Settings").clicked() {
                    let _ = crate::open_url(permissions::screen_recording_settings_url());
                }
            },
        );
        ui.add_space(8.0);
        permission_row(
            ui,
            "Accessibility",
            "Richest activity signal (window titles, focus).",
            ax,
            |ui| {
                if ui.button("Open Settings").clicked() {
                    let _ = crate::open_url(permissions::accessibility_settings_url());
                }
            },
        );

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("🔄  Re-check").clicked() {
                self.perms = permissions::status();
            }
            if self.perms.all_ok() {
                ui.colored_label(Color32::from_rgb(0x2d, 0xd4, 0xb0), "All set.");
            } else {
                ui.label(
                    egui::RichText::new(
                        "After granting in System Settings, click Re-check (a restart of the app may be needed for Screen Recording).",
                    )
                    .weak(),
                );
            }
        });

        egui::TopBottomPanel::bottom("perms_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("←  Back").clicked() {
                    self.step = Step::Connect;
                }
                if ui.button("Next  →").clicked() {
                    self.step = Step::Finish;
                }
            });
        });
    }

    fn finish_step(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.heading("🎉  You're set up");
        ui.add_space(8.0);

        let who = self
            .config
            .oidc_session
            .as_ref()
            .and_then(|s| s.email.clone().or_else(|| s.subject.clone()))
            .or_else(|| if self.has_credential() { Some("API key".into()) } else { None });
        match who {
            Some(w) => ui.label(format!("Connected to {} as {w}.", self.config.server.portal_url)),
            None => ui.colored_label(
                Color32::from_rgb(0xff, 0xb0, 0x50),
                "No credential configured — captures will queue locally until you sign in from the tray.",
            ),
        };

        if permissions::permissions_relevant() {
            let p = permissions::status();
            ui.add_space(6.0);
            ui.label(format!("Screen Recording: {}", p.screen_recording.badge()));
            ui.label(format!("Accessibility: {}", p.accessibility.badge()));
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new("Next: open Settings to add folders you want tracked — file tracking stays off until you do.").weak());

        egui::TopBottomPanel::bottom("finish_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("←  Back").clicked() {
                    self.step = if permissions::permissions_relevant() {
                        Step::Permissions
                    } else {
                        Step::Connect
                    };
                }
                if ui.button("Finish").clicked() {
                    let ctx = ui.ctx().clone();
                    self.finish(&ctx);
                }
            });
        });
    }
}

fn permission_row(
    ui: &mut egui::Ui,
    title: &str,
    why: &str,
    status: permissions::PermissionStatus,
    buttons: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.label(egui::RichText::new(why).weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                buttons(ui);
                let color = match status {
                    permissions::PermissionStatus::Granted => Color32::from_rgb(0x2d, 0xd4, 0xb0),
                    permissions::PermissionStatus::Denied => Color32::from_rgb(0xff, 0x6b, 0x6b),
                    permissions::PermissionStatus::NotRequired => Color32::GRAY,
                };
                ui.colored_label(color, status.badge());
            });
        });
    });
}

impl eframe::App for OnboardingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            let title = match self.step {
                Step::Welcome => "Welcome to CI-Applet",
                Step::Connect => "Connect to CI-Server",
                Step::Permissions => "Grant capture permissions",
                Step::Finish => "All set",
            };
            self.header(ui, title);
            match self.step {
                Step::Welcome => self.welcome(ui),
                Step::Connect => self.connect(ui),
                Step::Permissions => self.permissions(ui),
                Step::Finish => self.finish_step(ui),
            }
        });
    }
}
