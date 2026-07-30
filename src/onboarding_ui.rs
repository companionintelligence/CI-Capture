//! First-run onboarding wizard (`--onboarding-ui`).
//!
//! A four-step child-process window (Welcome → Connect → Permissions → Finish)
//! that gets a brand-new user from install to actually-capturing:
//!
//! - **Connect** — sign in to Companion Portal first (OIDC browser login), then
//!   confirm / autofill the CI-Server URL discovered from Portal when possible.
//! - **Permissions** — on macOS, live Screen Recording + Accessibility status
//!   with one-click *Grant* / *Open Settings* / *Re-check* aids (the grants the
//!   capture lanes silently need). Auto-skipped where the OS doesn't gate them.
//!
//! Runs as a separate process like the settings window, and sets
//! `onboarding_completed` on finish so it doesn't reappear.
//!
//! Visuals follow `theme` (ci-common tokens) so the card + primary CTA match
//! CI-Portal / CI-Hub sign-in density.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::NativeOptions;
use egui::Color32;
use parking_lot::Mutex;

use crate::config::{AppConfig, ConfigManager};
use crate::server_common::AuthCredential;
use crate::theme;
use crate::{connection_test, permissions};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Connect,
    Permissions,
    Finish,
}

/// Result of an async job (login, discovery, or connection test): Ok(detail) / Err(msg).
type JobResult = Result<String, String>;

pub fn run_onboarding_ui() -> Result<()> {
    let manager = Arc::new(ConfigManager::new().context("building config manager")?);
    let config = manager.load_or_default().context("loading configuration")?;
    let completed = Arc::new(Mutex::new(false));

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Welcome to CI-Applet")
            .with_inner_size([760.0, 640.0])
            .with_min_inner_size([680.0, 560.0]),
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
    show_advanced: bool,
    login_rx: Option<Receiver<JobResult>>,
    discover_rx: Option<Receiver<JobResult>>,
    test_rx: Option<Receiver<JobResult>>,
    status_line: Option<Result<String, String>>,
    server_autofilled: bool,

    // Permissions step
    perms: permissions::Permissions,
}

/// Prefill portal sign-in URL when the config's issuer is empty (the config
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
            show_advanced: false,
            login_rx: None,
            discover_rx: None,
            test_rx: None,
            status_line: None,
            server_autofilled: false,
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

    fn signed_into_portal(&self) -> bool {
        self.config.oidc_session.is_some()
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
                "Enter your portal sign-in URL first (Advanced), or paste an API key below."
                    .into(),
            ));
            return;
        }
        // Persist issuer before opening the browser; CI-Server URL can wait
        // until after Portal sign-in (and optional autofill).
        let issuer = self.oidc_issuer.trim().to_string();
        self.persist_merged(|c| {
            c.oidc.issuer = issuer;
        });
        let (tx, rx) = channel();
        self.login_rx = Some(rx);
        self.status_line = Some(Ok("Opening your browser to sign in…".into()));
        std::thread::spawn(move || {
            let result = crate::login_with_portal().map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    fn start_server_discovery(&mut self) {
        // Auto path after login: don't clobber a URL the user already entered.
        if !self.server_url.trim().is_empty() {
            return;
        }
        self.start_server_discovery_forced();
    }

    fn start_server_discovery_forced(&mut self) {
        let issuer = if self.config.oidc.issuer.trim().is_empty() {
            self.oidc_issuer.clone()
        } else {
            self.config.oidc.issuer.clone()
        };
        if issuer.trim().is_empty() {
            return;
        }
        // Prefer access_token for Portal REST; fall back to id_token.
        let bearer = self.config.oidc_session.as_ref().and_then(|s| {
            s.access_token.clone().or_else(|| s.id_token.clone())
        });
        let (tx, rx) = channel();
        self.discover_rx = Some(rx);
        self.status_line = Some(Ok("Looking up your CI-Server from Portal…".into()));
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("runtime: {e}")));
                    return;
                }
            };
            let result = runtime.block_on(async {
                match crate::portal_discover::discover_ci_server_url(&issuer, bearer.as_deref())
                    .await
                {
                    Ok(Some(url)) => Ok(url),
                    Ok(None) => Err(
                        "No CI-Server found on your Portal account yet — paste the URL below."
                            .into(),
                    ),
                    Err(e) => Err(format!(
                        "Could not auto-find CI-Server ({e:#}). Enter the URL below."
                    )),
                }
            });
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
        if url.is_empty() {
            self.status_line = Some(Err("Enter a CI-Server URL before saving an API key.".into()));
            return;
        }
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
                        self.status_line = Some(result);
                        self.start_server_discovery();
                    } else {
                        self.status_line = Some(result);
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.login_rx = None,
            }
        }
        if let Some(rx) = &self.discover_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.discover_rx = None;
                    match result {
                        Ok(url) => {
                            self.server_url = url.clone();
                            self.server_autofilled = true;
                            self.save_connection_fields();
                            self.status_line = Some(Ok(format!(
                                "Found your CI-Server and filled it in: {url}"
                            )));
                        }
                        Err(msg) => self.status_line = Some(Err(msg)),
                    }
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
                Err(TryRecvError::Disconnected) => self.discover_rx = None,
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
        self.login_rx.is_some() || self.discover_rx.is_some() || self.test_rx.is_some()
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
            ui.spacing_mut().item_spacing.x = 12.0;
            if let Some(logo) = &self.logo {
                theme::logo(ui, logo, 40.0, Color32::WHITE);
            }
            ui.vertical(|ui| {
                ui.add_space(2.0);
                theme::heading(ui, title);
            });
        });
        ui.add_space(8.0);
        // Step dots.
        let steps = [Step::Welcome, Step::Connect, Step::Permissions, Step::Finish];
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for s in steps {
                let on = s == self.step;
                ui.label(
                    egui::RichText::new(if on { "●" } else { "○" })
                        .font(theme::body(16.0))
                        .color(if on { theme::TEAL } else { theme::FAINT }),
                );
            }
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(10.0);
    }

    fn status_widget(&self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status_line {
            match status {
                Ok(msg) => {
                    ui.colored_label(
                        theme::GREEN,
                        egui::RichText::new(format!("✓ {msg}")).font(theme::body(14.0)),
                    );
                }
                Err(msg) => {
                    ui.colored_label(
                        theme::RED,
                        egui::RichText::new(format!("✗ {msg}")).font(theme::body(14.0)),
                    );
                }
            }
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(
            ui,
            "CI-Applet grows your Companion Memory by capturing your desktop and streaming it to your own CI-Server.",
        );
        ui.add_space(14.0);
        theme::card(ui, |ui| {
            ui.label(
                egui::RichText::new("It captures three things")
                    .font(theme::semibold(15.0))
                    .color(theme::TEXT),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Screenshots of your screen, on an interval (diff-gated).")
                    .font(theme::body(15.0))
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new("System activity — foreground app, window, working directory.")
                    .font(theme::body(15.0))
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new("Edits to folders you choose — so your AI remembers your files.")
                    .font(theme::body(15.0))
                    .color(theme::TEXT),
            );
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            ui.label(
                egui::RichText::new("Private by design")
                    .font(theme::semibold(15.0))
                    .color(theme::TEXT),
            );
            ui.add_space(6.0);
            theme::caption(ui, "Local-first — capture streams only to the CI-Server you sign in to.");
            theme::caption(ui, "File tracking is off until you add a folder yourself.");
            theme::caption(ui, "Credential stores (.ssh, .env, keychains) are never sent.");
        });
        ui.add_space(16.0);
        egui::TopBottomPanel::bottom("welcome_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Get started  →").clicked() {
                    self.step = Step::Connect;
                }
            });
        });
    }

    fn connect(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(
            ui,
            "Sign in to Companion Portal first — then confirm where captures should stream.",
        );
        ui.add_space(12.0);

        let running = self.job_running();

        // Portal sign-in card — mirrors CI-Portal / CI-Hub login density.
        theme::card(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(if self.signed_into_portal() {
                        "Signed in"
                    } else {
                        "Sign in"
                    })
                    .font(theme::light(28.0))
                    .color(theme::TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if self.signed_into_portal() {
                        "Portal account connected. Confirm your CI-Server below."
                    } else {
                        "Use your Companion Portal account — the same sign-in as Hub."
                    })
                    .font(theme::body(15.0))
                    .color(theme::MUTED),
                );
            });
            ui.add_space(16.0);

            ui.add_enabled_ui(!running, |ui| {
                if self.signed_into_portal() {
                    let who = self
                        .config
                        .oidc_session
                        .as_ref()
                        .and_then(|s| s.email.clone().or_else(|| s.name.clone()))
                        .unwrap_or_else(|| "your account".into());
                    theme::status_badge(ui, &format!("Signed in as {who}"), theme::GREEN);
                    ui.add_space(10.0);
                    if theme::ghost_button(ui, "Sign in again").clicked() {
                        self.start_login();
                    }
                } else if theme::primary_button_wide(ui, "Sign in with browser").clicked() {
                    self.start_login();
                }
            });

            if running && self.login_rx.is_some() {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    theme::caption(ui, "Waiting for browser sign-in…");
                });
            }
        });

        ui.add_space(16.0);

        // CI-Server URL — after portal, autofilled when Portal lists ci-memory.
        theme::field_label(ui, "CI-Server URL");
        ui.add_space(4.0);
        theme::text_field(
            ui,
            &mut self.server_url,
            "https://your-ci-server or http://core-1:8642",
            theme::FieldOpts::monospace(),
        );
        if self.server_autofilled {
            theme::caption(ui, "Autofilled from your Portal devices (ci-memory). You can edit it.");
        } else if self.signed_into_portal() {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!running, egui::Button::new("Find from Portal"))
                    .clicked()
                {
                    self.start_server_discovery_forced();
                }
                if self.discover_rx.is_some() {
                    ui.spinner();
                }
            });
        } else {
            theme::caption(
                ui,
                "Sign in above and we’ll try to fill this from your Portal devices.",
            );
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !running && self.has_credential() && !self.server_url.trim().is_empty(),
                    egui::Button::new("Test connection"),
                )
                .clicked()
            {
                self.start_connection_test();
            }
            if self.test_rx.is_some() {
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
            ui.label(egui::RichText::new(cred).font(theme::body(13.0)).color(theme::MUTED));
        });

        ui.add_space(10.0);
        self.status_widget(ui);

        ui.add_space(14.0);
        // Advanced: issuer override + API key fallback (collapsed like portal password fallback).
        if ui
            .button(if self.show_advanced {
                "Hide advanced options"
            } else {
                "Use API key or change portal URL…"
            })
            .clicked()
        {
            self.show_advanced = !self.show_advanced;
        }
        if self.show_advanced {
            ui.add_space(10.0);
            ui.add_enabled_ui(!running, |ui| {
                theme::field_label(ui, "Portal sign-in URL");
                ui.add_space(4.0);
                theme::text_field(
                    ui,
                    &mut self.oidc_issuer,
                    "https://your-portal/api/auth",
                    theme::FieldOpts::monospace(),
                );
                ui.add_space(12.0);
                theme::field_label(ui, "Or paste an API key");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    theme::text_field(
                        ui,
                        &mut self.api_key,
                        "CI-Server API key or bearer token",
                        theme::FieldOpts::password(),
                    );
                    if theme::ghost_button(ui, "Save key").clicked() {
                        self.start_api_key_save();
                    }
                });
            });
        }

        egui::TopBottomPanel::bottom("connect_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, "←  Back").clicked() {
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
                if self.has_credential() && !self.server_url.trim().is_empty() {
                    if theme::primary_button(ui, "Next  →").clicked() {
                        self.save_connection_fields();
                        self.perms = permissions::status();
                        self.step = next;
                    }
                } else {
                    if theme::ghost_button(ui, "Skip for now  →").clicked() {
                        self.step = next;
                    }
                    ui.label(
                        egui::RichText::new("You can sign in later from the tray.")
                            .font(theme::body(13.0))
                            .color(theme::MUTED),
                    );
                }
            });
        });
    }

    fn permissions(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(
            ui,
            "macOS needs two permissions for the applet to actually see your screen and activity. Without them, capture silently produces blank frames.",
        );
        ui.add_space(12.0);

        let sr = self.perms.screen_recording;
        let ax = self.perms.accessibility;

        permission_row(
            ui,
            "Screen Recording",
            "Required for screenshots.",
            sr,
            |ui| {
                if sr == permissions::PermissionStatus::Denied
                    && theme::ghost_button(ui, "Grant…").clicked()
                {
                    permissions::request_screen_recording();
                }
                if theme::ghost_button(ui, "Open Settings").clicked() {
                    let _ = crate::open_url(permissions::screen_recording_settings_url());
                }
            },
        );
        ui.add_space(10.0);
        permission_row(
            ui,
            "Accessibility",
            "Richest activity signal (window titles, focus).",
            ax,
            |ui| {
                if theme::ghost_button(ui, "Open Settings").clicked() {
                    let _ = crate::open_url(permissions::accessibility_settings_url());
                }
            },
        );

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if theme::ghost_button(ui, "Re-check").clicked() {
                self.perms = permissions::status();
            }
            if self.perms.all_ok() {
                ui.colored_label(
                    theme::GREEN,
                    egui::RichText::new("All set.").font(theme::body(15.0)),
                );
            } else {
                theme::caption(
                    ui,
                    "After granting in System Settings, click Re-check (a restart may be needed for Screen Recording).",
                );
            }
        });

        egui::TopBottomPanel::bottom("perms_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, "←  Back").clicked() {
                    self.step = Step::Connect;
                }
                if theme::primary_button(ui, "Next  →").clicked() {
                    self.step = Step::Finish;
                }
            });
        });
    }

    fn finish_step(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        theme::heading(ui, "You're set up");
        ui.add_space(10.0);

        let who = self
            .config
            .oidc_session
            .as_ref()
            .and_then(|s| s.email.clone().or_else(|| s.subject.clone()))
            .or_else(|| if self.has_credential() { Some("API key".into()) } else { None });
        match who {
            Some(w) => ui.label(
                egui::RichText::new(format!(
                    "Connected to {} as {w}.",
                    self.config.server.portal_url
                ))
                .font(theme::body(15.0))
                .color(theme::TEXT),
            ),
            None => ui.colored_label(
                theme::AMBER,
                egui::RichText::new(
                    "No credential configured — captures will queue locally until you sign in from the tray.",
                )
                .font(theme::body(15.0)),
            ),
        };

        if permissions::permissions_relevant() {
            let p = permissions::status();
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("Screen Recording: {}", p.screen_recording.badge()))
                    .font(theme::body(15.0))
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new(format!("Accessibility: {}", p.accessibility.badge()))
                    .font(theme::body(15.0))
                    .color(theme::TEXT),
            );
        }

        ui.add_space(12.0);
        theme::caption(
            ui,
            "Next: open Settings to add folders you want tracked — file tracking stays off until you do.",
        );

        egui::TopBottomPanel::bottom("finish_nav").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::ghost_button(ui, "←  Back").clicked() {
                    self.step = if permissions::permissions_relevant() {
                        Step::Permissions
                    } else {
                        Step::Connect
                    };
                }
                if theme::primary_button(ui, "Finish").clicked() {
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
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::semibold(15.0))
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(why)
                        .font(theme::body(13.0))
                        .color(theme::MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                buttons(ui);
                let color = match status {
                    permissions::PermissionStatus::Granted => theme::GREEN,
                    permissions::PermissionStatus::Denied => theme::RED,
                    permissions::PermissionStatus::NotRequired => theme::FAINT,
                };
                ui.colored_label(
                    color,
                    egui::RichText::new(status.badge()).font(theme::semibold(13.0)),
                );
            });
        });
    });
}

impl eframe::App for OnboardingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(24))
                .show(ui, |ui| {
                    let title = match self.step {
                        Step::Welcome => "Welcome to CI-Applet",
                        Step::Connect => "Connect your account",
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
        });
    }
}
