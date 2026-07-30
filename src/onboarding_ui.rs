//! First-run onboarding wizard (`--onboarding-ui`).
//!
//! A child-process window (What it does → Connect → Permission → Done) that
//! gets a brand-new user from install to actually-capturing, with as little
//! typing as possible:
//!
//! - **What it does** — an animated explainer ([`theme::capture_flow`]) rather
//!   than a wall of bullets, because the first question is always "what is this
//!   thing actually recording?"
//! - **Connect** — one button. Browser sign-in with the Portal, after which the
//!   Core's address is discovered rather than typed. Manual entry still exists
//!   but is collapsed behind a disclosure; a fresh install should never open it.
//! - **Permission** — macOS Screen Recording only, polled once a second so the
//!   window notices the grant on its own. Skipped where the OS doesn't gate
//!   capture. Accessibility is deliberately *not* requested: nothing in the
//!   applet or in `vizier` reads an Accessibility API (see [`permissions`]).
//!
//! Runs as a separate process like the settings window, and sets
//! `onboarding_completed` on finish so it doesn't reappear.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::NativeOptions;
use parking_lot::Mutex;

use crate::config::{AppConfig, ConfigManager};
use crate::{connection_test, permissions, theme, tokens};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Connect,
    Permissions,
    Finish,
}

impl Step {
    /// Steps in order. `Permissions` is skipped at runtime where the OS does not
    /// gate capture, so the chip row is built from [`Step::visible`].
    const ALL: [Step; 4] = [
        Step::Welcome,
        Step::Connect,
        Step::Permissions,
        Step::Finish,
    ];

    fn short_label(self) -> &'static str {
        match self {
            Step::Welcome => "What it does",
            Step::Connect => "Connect",
            Step::Permissions => "Permission",
            Step::Finish => "Done",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Companion Memory",
            Step::Connect => "Connect to your Core",
            Step::Permissions => "One permission",
            Step::Finish => "You're set",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The next step, skipping `Permissions` on platforms that don't gate capture.
    fn next(self) -> Option<Step> {
        let mut i = self.index() + 1;
        while let Some(&candidate) = Self::ALL.get(i) {
            if candidate == Step::Permissions && !permissions::permissions_relevant() {
                i += 1;
                continue;
            }
            return Some(candidate);
        }
        None
    }

    fn prev(self) -> Option<Step> {
        let mut i = self.index();
        while i > 0 {
            i -= 1;
            let candidate = Self::ALL[i];
            if candidate == Step::Permissions && !permissions::permissions_relevant() {
                continue;
            }
            return Some(candidate);
        }
        None
    }
}

/// Result of an async job (login or connection test): Ok(detail) / Err(msg).
type JobResult = Result<String, String>;

pub fn run_onboarding_ui() -> Result<()> {
    let manager = Arc::new(ConfigManager::new().context("building config manager")?);
    let config = manager.load_or_default().context("loading configuration")?;
    let completed = Arc::new(Mutex::new(false));

    let native_options = NativeOptions {
        centered: true,
        viewport: egui::ViewportBuilder::default()
            .with_title("Welcome to Companion Memory")
            .with_inner_size([840.0, 680.0])
            .with_min_inner_size([720.0, 600.0]),
        ..Default::default()
    };

    let manager_c = manager.clone();
    let completed_c = completed.clone();
    eframe::run_native(
        "Companion Memory Setup",
        native_options,
        Box::new(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            crate::theme::apply_visuals(&cc.egui_ctx);
            let logo = crate::theme::load_logo(&cc.egui_ctx);
            Ok(Box::new(OnboardingApp::new(
                manager_c.clone(),
                config.clone(),
                completed_c.clone(),
                logo,
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
    logo: egui::TextureHandle,
    step: Step,
    /// One-shot latch for [`theme::center_once`].
    centered: bool,
    /// Last time OS permissions were re-read, in egui's monotonic seconds.
    perms_polled_at: f64,

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

/// Production Portal sign-in URL.
///
/// Used as the *effective* default rather than a prefilled text box, so a fresh
/// install can sign in with zero typing. `hub.companionintelligence.com` is the
/// dev worker per CI-Portal's `wrangler.jsonc`; `hub.ci.computer` is production
/// and is what CI-Browser-Extension ships with.
const DEFAULT_ISSUER: &str = "https://hub.ci.computer/api/auth";

impl OnboardingApp {
    fn new(
        manager: Arc<ConfigManager>,
        config: AppConfig,
        completed: Arc<Mutex<bool>>,
        logo: egui::TextureHandle,
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
            centered: false,
            perms_polled_at: 0.0,
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
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
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
            ui.spacing_mut().item_spacing.x = 12.0;
            // Tinted, not raw: `ci-mark-white.svg` is 18 white-filled paths, so
            // drawing it untinted leaves it invisible on any light surface.
            theme::logo(ui, &self.logo, 34.0, theme::TEAL);
            theme::heading(ui, title);
        });
        ui.add_space(14.0);

        // Named steps, not anonymous dots — two of them, so there is no reason
        // to make the user guess how far along they are.
        let here = self.step.index();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (i, label) in Step::ALL.iter().map(|s| s.short_label()).enumerate() {
                let (fill, fg) = match i.cmp(&here) {
                    std::cmp::Ordering::Less => (theme::tint(theme::TEAL, 40), theme::TEAL),
                    std::cmp::Ordering::Equal => (theme::TEAL, theme::ON_TEAL),
                    std::cmp::Ordering::Greater => (theme::CARD, theme::FAINT),
                };
                let text = if i < here {
                    format!("✓  {label}")
                } else {
                    format!("{}  {label}", i + 1)
                };
                let galley =
                    ui.painter()
                        .layout_no_wrap(text.clone(), theme::body(tokens::text::SMALL), fg);
                let size = galley.size() + egui::vec2(20.0, 10.0);
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(255), fill);
                ui.painter()
                    .galley(rect.center() - galley.size() / 2.0, galley, fg);
            }
        });
        ui.add_space(16.0);
    }

    fn status_widget(&self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status_line {
            let (glyph, color, msg) = match status {
                Ok(m) => ("✓", theme::GREEN, m),
                Err(m) => ("✗", theme::RED, m),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("{glyph}  {msg}"))
                        .font(theme::body(tokens::text::LABEL))
                        .color(color),
                );
            });
        }
    }

    // ── step 1: what this is, and one button to connect ─────────────────────

    fn welcome(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(
            ui,
            "Everything you see and edit on this computer becomes searchable memory — held on \
             your own Core, not in anyone else's cloud.",
        );
        ui.add_space(14.0);

        // The animated explainer: three lanes flowing to your Core.
        theme::capture_flow(ui, 150.0);
        ui.add_space(18.0);

        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            for (color, title, body) in [
                (
                    theme::LANE_SHOT,
                    "Screens",
                    "A picture of your screen now and then. Near-identical frames are skipped.",
                ),
                (
                    theme::LANE_ACTIVITY,
                    "Activity",
                    "Which app and window you're in, so your memory has context.",
                ),
                (
                    theme::LANE_FILE,
                    "Files",
                    "Documents you edit in folders you choose. Off until you pick one.",
                ),
            ] {
                ui.horizontal(|ui| {
                    theme::status_dot(ui, color, false);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            egui::RichText::new(title)
                                .font(theme::semibold(tokens::text::LABEL))
                                .color(theme::TEXT),
                        );
                        theme::caption(ui, body);
                    });
                });
                ui.add_space(10.0);
            }
        });

        ui.add_space(14.0);
        theme::caption(
            ui,
            "Passwords, SSH keys, .env files and keychains are never read — that exclusion can't \
             be turned off.",
        );
    }

    // ── step 2: connect, with everything automatic ──────────────────────────

    fn connect(&mut self, ui: &mut egui::Ui) {
        let running = self.job_running();

        theme::subtitle(
            ui,
            "Sign in and we'll find your Core for you. Nothing to type.",
        );
        ui.add_space(16.0);

        if self.has_credential() {
            theme::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::status_dot(ui, theme::GREEN, true);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(
                            egui::RichText::new(self.who().unwrap_or_else(|| "Signed in".into()))
                                .font(theme::semibold(tokens::text::BODY))
                                .color(theme::TEXT),
                        );
                        let target = if self.server_url.trim().is_empty() {
                            "Looking for your Core…".to_string()
                        } else {
                            self.server_url.trim().to_string()
                        };
                        theme::mono_label(ui, &target, theme::MUTED);
                    });
                });
            });
            ui.add_space(12.0);
        } else {
            ui.add_enabled_ui(!running, |ui| {
                if theme::primary_button(ui, "Continue with Portal").clicked() {
                    self.start_login();
                }
            });
            ui.add_space(8.0);
            theme::caption(
                ui,
                "Opens your browser to sign in. We never see your password.",
            );
            ui.add_space(14.0);
        }

        if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(6.0);
                theme::caption(ui, "Working…");
            });
            ui.add_space(10.0);
        }

        self.status_widget(ui);
        ui.add_space(12.0);

        // Everything below is the escape hatch, not the happy path. A fresh
        // install should never need to open it — which is why it is collapsed
        // and why nothing here is prefilled with someone else's hostname.
        egui::CollapsingHeader::new(
            egui::RichText::new("Set things up manually")
                .font(theme::body(tokens::text::SMALL))
                .color(theme::MUTED),
        )
        .id_salt("onboarding_advanced")
        .show(ui, |ui| {
            ui.add_space(6.0);
            theme::field_label(ui, "Your Core's address");
            theme::text_field(
                ui,
                &mut self.server_url,
                "http://core-1:8642 or http://my-core.tailnet.ts.net:8642",
                theme::FieldOpts::default(),
            );
            theme::caption(
                ui,
                "A LAN hostname or a Tailscale address — wherever your Core runs.",
            );
            ui.add_space(12.0);

            theme::field_label(ui, "Portal sign-in URL");
            theme::text_field(
                ui,
                &mut self.oidc_issuer,
                DEFAULT_ISSUER,
                theme::FieldOpts::default(),
            );
            ui.add_space(12.0);

            theme::field_label(ui, "…or paste an access key");
            ui.horizontal(|ui| {
                theme::text_field(
                    ui,
                    &mut self.api_key,
                    "Access key for your Core",
                    theme::FieldOpts {
                        password: true,
                        ..Default::default()
                    },
                );
                if theme::ghost_button(ui, "Save").clicked() {
                    self.start_api_key_save();
                }
            });
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !running && self.has_credential(),
                        egui::Button::new("Test the connection"),
                    )
                    .clicked()
                {
                    self.start_connection_test();
                }
            });
            ui.add_space(6.0);
        });
    }

    // ── step 3 (macOS only): the one permission that matters ────────────────

    fn permissions(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(
            ui,
            "macOS needs your permission before anything can see the screen.",
        );
        ui.add_space(16.0);

        let sr = self.perms.screen_recording;
        let granted = sr.is_ok();

        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::status_dot(
                    ui,
                    if granted { theme::GREEN } else { theme::AMBER },
                    !granted,
                );
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(
                        egui::RichText::new("Screen Recording")
                            .font(theme::semibold(tokens::text::BODY))
                            .color(theme::TEXT),
                    );
                    theme::caption(
                        ui,
                        if granted {
                            "Granted — screens and activity will be captured."
                        } else {
                            "Without this, captures come out blank."
                        },
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !granted {
                        if theme::primary_button(ui, "Allow…").clicked() {
                            // Triggers the OS prompt on first call; afterwards
                            // macOS only honours the Settings pane.
                            permissions::request_screen_recording();
                            let _ = crate::open_url(permissions::screen_recording_settings_url());
                        }
                    } else {
                        theme::status_badge(ui, "Granted", theme::GREEN);
                    }
                });
            });
        });

        ui.add_space(14.0);
        if granted {
            ui.label(
                egui::RichText::new("✓  Nothing else to do.")
                    .font(theme::body(tokens::text::LABEL))
                    .color(theme::GREEN),
            );
        } else {
            theme::caption(
                ui,
                "Flip the switch next to Companion Memory in System Settings — this page notices \
                 on its own, no need to come back and click anything.",
            );
        }

        // Accessibility is deliberately absent. Nothing in the applet or in
        // vizier reads a single Accessibility API — window titles come from
        // CGWindowListCopyWindowInfo, which is gated by Screen Recording. Asking
        // for it pinned the tray to "⚠︎ Permissions needed" on working installs.
    }

    // ── step 4: what's on, and what to do next ──────────────────────────────

    fn finish_step(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(ui, "Capture is running. Here's where things stand.");
        ui.add_space(16.0);

        let connected = self.has_credential();
        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            self.summary_row(
                ui,
                connected,
                "Connected to your Core",
                &match self.who() {
                    Some(w) if connected => format!("{w} · {}", self.server_url.trim()),
                    _ => "Not signed in — captures will wait on this machine until you are."
                        .to_string(),
                },
            );
            ui.add_space(10.0);

            if permissions::permissions_relevant() {
                let sr = permissions::status().screen_recording;
                self.summary_row(
                    ui,
                    sr.is_ok(),
                    "Screen Recording allowed",
                    if sr.is_ok() {
                        "Screens and activity are being captured."
                    } else {
                        "Captures will be blank until you allow it in System Settings."
                    },
                );
                ui.add_space(10.0);
            }

            let folders = self.config.file_tracker.watched_paths.len();
            self.summary_row(
                ui,
                folders > 0,
                "Watching folders for file edits",
                &match folders {
                    0 => "Off. Add a folder in Settings whenever you want it.".to_string(),
                    1 => "1 folder.".to_string(),
                    n => format!("{n} folders."),
                },
            );
        });

        ui.add_space(18.0);
        theme::field_label(ui, "Where to find things");
        ui.add_space(8.0);
        theme::caption(
            ui,
            "Companion Memory lives in your menu bar. Click it to pause capture, open your \
             memory in a browser, add folders, or sign in again.",
        );
    }

    /// One ✓/• line in the finish summary.
    fn summary_row(&self, ui: &mut egui::Ui, ok: bool, title: &str, detail: &str) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(if ok { "✓" } else { "•" })
                    .font(theme::semibold(tokens::text::BODY))
                    .color(if ok { theme::GREEN } else { theme::AMBER }),
            );
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::body(tokens::text::LABEL))
                        .color(theme::TEXT),
                );
                theme::caption(ui, detail);
            });
        });
    }

    /// Who we're signed in as, if anyone.
    fn who(&self) -> Option<String> {
        self.config
            .oidc_session
            .as_ref()
            .and_then(|s| s.email.clone().or_else(|| s.subject.clone()))
            .or_else(|| {
                self.has_credential()
                    .then(|| "Signed in with an access key".into())
            })
    }

    /// Re-read OS permission state on a timer while the Permissions step is up.
    ///
    /// The user grants Screen Recording in System Settings, i.e. in a different
    /// application — so making them come back and press "Re-check" is asking
    /// them to do the computer's job. Poll instead and advance on our own.
    fn poll_permissions(&mut self, ctx: &egui::Context) {
        if self.step != Step::Permissions {
            return;
        }
        let now = ctx.input(|i| i.time);
        if now - self.perms_polled_at >= 1.0 {
            self.perms_polled_at = now;
            self.perms = permissions::status();
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// The single persistent navigation bar.
    fn nav_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(prev) = self.step.prev() {
                if theme::ghost_button(ui, "←  Back").clicked() {
                    self.step = prev;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.step {
                    Step::Finish => {
                        if theme::primary_button(ui, "Start capturing").clicked() {
                            let ctx = ui.ctx().clone();
                            self.finish(&ctx);
                        }
                    }
                    Step::Connect => {
                        // Never strand the user: there is always exactly one
                        // forward control, so Finish (which sets
                        // onboarding_completed and stops the auto-reopen) is
                        // always reachable even without a credential.
                        let label = if self.has_credential() {
                            "Continue  →"
                        } else {
                            "Skip for now  →"
                        };
                        if theme::primary_button(ui, label).clicked() {
                            self.save_connection_fields();
                            self.perms = permissions::status();
                            if let Some(next) = self.step.next() {
                                self.step = next;
                            }
                        }
                        if !self.has_credential() {
                            theme::caption(ui, "You can sign in later from the menu bar.");
                        }
                    }
                    _ => {
                        if let Some(next) = self.step.next() {
                            let label = if self.step == Step::Welcome {
                                "Get started  →"
                            } else {
                                "Continue  →"
                            };
                            if theme::primary_button(ui, label).clicked() {
                                self.step = next;
                            }
                        }
                    }
                }
            });
        });
    }
}

impl eframe::App for OnboardingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs(ctx);
        theme::center_once(ctx, &mut self.centered);
        // Explicit fill: a bare `CentralPanel` inherits `Visuals::panel_fill`,
        // which is the one thing that silently reverts if theme registration
        // ever regresses. The other windows set it too — match them.
        self.poll_permissions(ctx);

        // ONE bottom panel with ONE stable id for every step. Four per-step
        // panels made the nav bar jump as the user advanced.
        egui::TopBottomPanel::bottom("onboarding_nav")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL_HI)
                    .inner_margin(egui::Margin::symmetric(30, 16)),
            )
            .show(ctx, |ui| self.nav_bar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(30, 26)),
            )
            .show(ctx, |ui| {
                let step = self.step;
                self.header(ui, step.title());
                // Every step scrolls. Without this the Connect step clipped
                // irrecoverably at the minimum window size, with no way to reach
                // the controls below the fold.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match step {
                        Step::Welcome => self.welcome(ui),
                        Step::Connect => self.connect(ui),
                        Step::Permissions => self.permissions(ui),
                        Step::Finish => self.finish_step(ui),
                    });
            });
    }
}
