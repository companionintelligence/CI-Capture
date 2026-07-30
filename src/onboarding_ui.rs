//! First-run onboarding wizard (`--onboarding-ui`).
//!
//! A four-step child-process window (Welcome → Connect → Permissions → Finish)
//! that gets a brand-new user from install to actually-capturing:
//!
//! - **Connect** — asks for **one** thing, the CI-Server URL. The OIDC issuer
//!   is derived (`<server>/api/auth`) and probed automatically, so the common
//!   deployment needs no second answer; the manual issuer field lives under
//!   *Advanced* for the rest. Reachability is probed as soon as the URL is
//!   entered — before any credential exists — because a URL typo is the most
//!   likely first mistake and shouldn't be discoverable only after a failed
//!   browser login.
//! - **Permissions** — on macOS, live Screen Recording + Accessibility status
//!   with one-click *Grant* / *Open Settings* aids, re-polled on a timer so
//!   returning from System Settings just shows green. Auto-skipped where the OS
//!   doesn't gate them.
//! - **Finish** — sends a real test capture through the production ingest path,
//!   so setup ends on *verified*, not *configured*.
//!
//! Runs as a separate process like the settings window. Exits with
//! [`RESTART_EXIT_CODE`] when the user asks to restart the app (the macOS
//! Screen Recording grant only takes effect on relaunch); the tray parent
//! interprets that code and re-execs itself.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use eframe::NativeOptions;
use egui::Color32;
use parking_lot::Mutex;

use crate::config::{AppConfig, ConfigManager};
use crate::server_common::{AuthCredential, derive_issuer_url, normalize_ci_server_url};
use crate::{connection_test, permissions};

/// Exit code meaning "the user asked to restart CI-Applet". Chosen to be
/// distinct from the 0/1 a normal run or crash produces.
pub const RESTART_EXIT_CODE: i32 = 42;

/// How often the Permissions step re-reads the OS grant state. Grants land
/// while the user is in System Settings, so polling is what makes the panel
/// turn green on its own instead of needing a manual re-check.
const PERMISSION_POLL: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Connect,
    Permissions,
    Finish,
}

/// Result of an async job: Ok(detail) / Err(friendly message).
type JobResult = Result<String, String>;

/// Completion of one background job. A single channel carries all of them so
/// the polling logic doesn't fan out per job kind.
enum JobMsg {
    /// Combined URL probe: server reachability + sign-in provider discovery.
    Probe {
        server: JobResult,
        issuer: JobResult,
    },
    Login(JobResult),
    Test(JobResult),
    Capture(JobResult),
}

/// Prefilled portal sign-in URL when no CI-Server URL has been entered yet and
/// the config's issuer is empty. Once a server URL exists the issuer is derived
/// from it instead.
const DEFAULT_ISSUER: &str = "https://hub.companionintelligence.com/api/auth";

pub fn run_onboarding_ui() -> Result<()> {
    let manager = Arc::new(ConfigManager::new().context("building config manager")?);
    let config = manager.load_or_default().context("loading configuration")?;
    let completed = Arc::new(Mutex::new(false));
    let restart_requested = Arc::new(Mutex::new(false));

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Welcome to CI-Applet")
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([640.0, 520.0]),
        ..Default::default()
    };

    let manager_c = manager.clone();
    let completed_c = completed.clone();
    let restart_c = restart_requested.clone();
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
                restart_c.clone(),
                Some(logo),
            )))
        }),
    )
    .map_err(|err| anyhow!("failed to run onboarding UI: {err}"))?;

    // The tray parent watches for this code and relaunches itself.
    if *restart_requested.lock() {
        std::process::exit(RESTART_EXIT_CODE);
    }

    Ok(())
}

struct OnboardingApp {
    manager: Arc<ConfigManager>,
    config: AppConfig,
    completed: Arc<Mutex<bool>>,
    restart_requested: Arc<Mutex<bool>>,
    logo: Option<egui::TextureHandle>,
    step: Step,

    // Connect step
    server_url: String,
    /// Manual issuer override. Empty means "derive from the server URL".
    oidc_issuer: String,
    /// Set once the user edits the issuer by hand, so re-deriving from a later
    /// server-URL edit doesn't silently discard their value.
    issuer_manual: bool,
    /// The URL field has unprobed edits (egui reports `changed()` and
    /// `lost_focus()` on different frames, so the edit has to be remembered).
    url_dirty: bool,
    show_advanced: bool,
    api_key: String,
    /// Latest reachability probe of the CI-Server URL.
    server_probe: Option<JobResult>,
    /// Latest discovery probe of the effective issuer.
    issuer_probe: Option<JobResult>,

    // Permissions step
    perms: permissions::Permissions,
    next_perm_poll: Option<Instant>,

    // Finish step
    capture_result: Option<JobResult>,

    // Shared async plumbing
    job_rx: Option<Receiver<JobMsg>>,
    status_line: Option<JobResult>,
}

impl OnboardingApp {
    fn new(
        manager: Arc<ConfigManager>,
        config: AppConfig,
        completed: Arc<Mutex<bool>>,
        restart_requested: Arc<Mutex<bool>>,
        logo: Option<egui::TextureHandle>,
    ) -> Self {
        // A stored issuer that isn't what we'd derive from the server URL is a
        // deliberate choice (split portal/server deployment) — preserve it and
        // show Advanced pre-expanded so it's visible, not silently overridden.
        let stored_issuer = config.oidc.issuer.trim().to_string();
        let derived = derive_issuer_url(&config.server.portal_url);
        let issuer_manual =
            !stored_issuer.is_empty() && Some(stored_issuer.as_str()) != derived.as_deref();

        Self {
            server_url: config.server.portal_url.clone(),
            oidc_issuer: if issuer_manual {
                stored_issuer
            } else {
                String::new()
            },
            issuer_manual,
            url_dirty: false,
            show_advanced: issuer_manual,
            api_key: String::new(),
            server_probe: None,
            issuer_probe: None,
            perms: permissions::status(),
            next_perm_poll: None,
            capture_result: None,
            job_rx: None,
            status_line: None,
            manager,
            config,
            completed,
            restart_requested,
            logo,
            step: Step::Welcome,
        }
    }

    fn has_credential(&self) -> bool {
        crate::server_ingest::is_configured(&self.config)
    }

    /// The issuer this wizard will actually sign in against: the manual
    /// override when set, otherwise `<server>/api/auth`, otherwise the
    /// well-known default (for users who have no server URL yet).
    fn effective_issuer(&self) -> String {
        if self.issuer_manual && !self.oidc_issuer.trim().is_empty() {
            return self.oidc_issuer.trim().to_string();
        }
        derive_issuer_url(&self.server_url).unwrap_or_else(|| DEFAULT_ISSUER.to_string())
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

    /// Normalize the typed URL, write it back into the field so the user sees
    /// what was actually stored, and persist it alongside the effective issuer.
    fn save_connection_fields(&mut self) {
        if let Ok(normalized) = normalize_ci_server_url(&self.server_url) {
            self.server_url = normalized;
        }
        let url = self.server_url.trim().to_string();
        let issuer = self.effective_issuer();
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

    /// Start a background job on its own thread + runtime, replacing any
    /// in-flight one. All jobs report through the single `job_rx` channel.
    fn spawn_job(&mut self, job: impl FnOnce(&Sender<JobMsg>) + Send + 'static) {
        let (tx, rx) = channel();
        self.job_rx = Some(rx);
        std::thread::spawn(move || job(&tx));
    }

    /// Probe the typed CI-Server URL: reachability plus sign-in discovery.
    /// Runs without a credential — the point is to catch a bad URL *before*
    /// the user attempts a login that was doomed from the start.
    fn start_probe(&mut self) {
        if self.server_url.trim().is_empty() {
            self.server_probe = None;
            self.issuer_probe = None;
            return;
        }
        self.save_connection_fields();
        let config = self.config.clone();
        let issuer = self.effective_issuer();
        self.server_probe = None;
        self.issuer_probe = None;
        self.spawn_job(move |tx| {
            let result = block_on(async {
                let server = connection_test::test_ci_server(&config).await;
                let issuer = connection_test::test_oidc_issuer(&issuer).await;
                (server, issuer)
            });
            let msg = match result {
                Ok((server, issuer)) => JobMsg::Probe { server, issuer },
                Err(e) => JobMsg::Probe {
                    server: Err(e.clone()),
                    issuer: Err(e),
                },
            };
            let _ = tx.send(msg);
        });
    }

    fn start_login(&mut self) {
        if self.effective_issuer().trim().is_empty() {
            self.status_line = Some(Err(
                "Enter your CI-Server URL first, or paste an API key below.".into(),
            ));
            return;
        }
        self.save_connection_fields();
        self.status_line = Some(Ok("Opening your browser to sign in…".into()));
        self.spawn_job(|tx| {
            // login_with_portal opens the browser, waits for the loopback
            // redirect, and persists the tokens to config.json.
            let result = crate::login_with_portal().map_err(|err| {
                // The raw chain is for the log; the user gets the actionable
                // version.
                log::warn!("portal sign-in failed: {err:#}");
                crate::oidc::friendly_login_error(&err)
            });
            let _ = tx.send(JobMsg::Login(result));
        });
    }

    fn start_api_key_save(&mut self) {
        let key = self.api_key.trim().to_string();
        if key.is_empty() {
            self.status_line = Some(Err("Paste an API key first.".into()));
            return;
        }
        if let Ok(normalized) = normalize_ci_server_url(&self.server_url) {
            self.server_url = normalized;
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
        let config = self.config.clone();
        self.spawn_job(move |tx| {
            let result = block_on(connection_test::test_ci_server(&config)).unwrap_or_else(Err);
            let _ = tx.send(JobMsg::Test(result));
        });
    }

    /// Push one real screenshot through the production ingest path.
    fn start_test_capture(&mut self) {
        self.reload_config();
        let config = self.config.clone();
        self.capture_result = None;
        self.status_line = Some(Ok("Capturing and uploading…".into()));
        self.spawn_job(move |tx| {
            let result = block_on(connection_test::test_capture(&config))
                .unwrap_or_else(Err);
            let _ = tx.send(JobMsg::Capture(result));
        });
    }

    /// Drain a finished job into the relevant slot.
    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.job_rx else { return };
        match rx.try_recv() {
            Ok(msg) => {
                self.job_rx = None;
                match msg {
                    JobMsg::Probe { server, issuer } => {
                        self.server_probe = Some(server);
                        self.issuer_probe = Some(issuer);
                    }
                    JobMsg::Login(result) => {
                        if result.is_ok() {
                            self.reload_config();
                        }
                        self.status_line = Some(result);
                    }
                    JobMsg::Test(result) => self.status_line = Some(result),
                    JobMsg::Capture(result) => {
                        self.status_line = None;
                        self.capture_result = Some(result);
                    }
                }
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
            Err(TryRecvError::Disconnected) => self.job_rx = None,
        }
    }

    fn job_running(&self) -> bool {
        self.job_rx.is_some()
    }

    /// Re-read OS permission state on a timer while the Permissions step is up,
    /// so a grant made in System Settings shows without a manual re-check.
    fn poll_permissions(&mut self, ctx: &egui::Context) {
        if self.step != Step::Permissions {
            self.next_perm_poll = None;
            return;
        }
        let now = Instant::now();
        match self.next_perm_poll {
            Some(at) if at > now => {}
            _ => {
                self.perms = permissions::status();
                self.next_perm_poll = Some(now + PERMISSION_POLL);
            }
        }
        ctx.request_repaint_after(PERMISSION_POLL);
    }

    fn finish(&mut self, ctx: &egui::Context) {
        self.reload_config();
        // Only a credential-bearing setup — or an explicit "finish without
        // connecting" — counts as done. Closing the window (no Finish click)
        // leaves the flag false so the wizard returns next launch.
        self.config.onboarding_completed = true;
        let _ = self.manager.save(&self.config);
        *self.completed.lock() = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn request_restart(&mut self, ctx: &egui::Context) {
        self.reload_config();
        self.config.onboarding_completed = true;
        let _ = self.manager.save(&self.config);
        *self.completed.lock() = true;
        *self.restart_requested.lock() = true;
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
            result_label(ui, status);
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
                    // Probe whatever is already configured so returning users
                    // land on a step that already knows if it works.
                    if !self.server_url.trim().is_empty() {
                        self.start_probe();
                    }
                }
            });
        });
    }

    fn connect(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label("Point the applet at your CI-Server. Everything else is worked out from it.");
        ui.add_space(8.0);

        let running = self.job_running();

        ui.label(egui::RichText::new("CI-Server URL").strong());
        let url_field = ui.add(
            egui::TextEdit::singleline(&mut self.server_url)
                .hint_text("https://your-ci-server  ·  or  core-1:8642")
                .desired_width(f32::INFINITY),
        );
        // Probe on blur or Enter. `changed()` is only true on the frame the
        // text actually changes — never the frame focus is lost — so the edit
        // is remembered in `url_dirty` and consumed here.
        if url_field.changed() {
            self.url_dirty = true;
        }
        let submitted = url_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (url_field.lost_focus() && self.url_dirty) || submitted {
            self.url_dirty = false;
            self.start_probe();
        }

        // Probe results: reachability and the derived sign-in provider.
        ui.add_space(4.0);
        if running && self.server_probe.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Checking…").weak());
            });
        } else {
            if let Some(server) = &self.server_probe {
                result_label(ui, server);
            }
            if let Some(issuer) = &self.issuer_probe {
                match issuer {
                    Ok(msg) => {
                        ui.colored_label(Color32::from_rgb(0x2d, 0xd4, 0xb0), format!("✓ {msg}"))
                    }
                    // A failed issuer probe is not fatal: API-key auth doesn't
                    // need one. Say so rather than showing a red error.
                    Err(_) => ui.colored_label(
                        Color32::from_rgb(0xff, 0xb0, 0x50),
                        "• No sign-in provider found at this URL — use an API key below, or set \
                         the sign-in URL under Advanced.",
                    ),
                };
            }
        }

        ui.add_space(10.0);
        ui.add_enabled_ui(!running, |ui| {
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

            ui.add_space(6.0);
            let issuer_preview = self.effective_issuer();
            // `default_open`, not `open`: forcing the state every frame would
            // make the header impossible to collapse.
            egui::CollapsingHeader::new("Advanced")
                .default_open(self.show_advanced)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Portal sign-in URL (OIDC issuer)").strong());
                    ui.label(
                        egui::RichText::new(
                            "Leave blank to derive it from the CI-Server URL above.",
                        )
                        .weak(),
                    );
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.oidc_issuer)
                            .hint_text(issuer_preview.clone())
                            .desired_width(f32::INFINITY),
                    );
                    if field.changed() {
                        self.issuer_manual = !self.oidc_issuer.trim().is_empty();
                    }
                    if field.lost_focus() {
                        self.start_probe();
                    }
                    ui.label(
                        egui::RichText::new(format!("Will sign in against: {issuer_preview}"))
                            .weak(),
                    );
                });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // Ungated: probing reachability without a credential is exactly how
            // a URL typo gets caught early.
            if ui
                .add_enabled(
                    !running && !self.server_url.trim().is_empty(),
                    egui::Button::new("Test connection"),
                )
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
                // stranded (and the wizard can always reach Finish).
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
                    ui.label(egui::RichText::new("You can sign in later from the tray.").weak());
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
                if sr == permissions::PermissionStatus::Denied && ui.button("Grant…").clicked() {
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
                if ax == permissions::PermissionStatus::Denied && ui.button("Grant…").clicked() {
                    // Raises the AX prompt; the grant itself lands when the
                    // user flips the toggle, which the poller picks up.
                    permissions::request_accessibility();
                }
                if ui.button("Open Settings").clicked() {
                    let _ = crate::open_url(permissions::accessibility_settings_url());
                }
            },
        );

        ui.add_space(12.0);
        if self.perms.all_ok() {
            ui.colored_label(
                Color32::from_rgb(0x2d, 0xd4, 0xb0),
                "All set — this panel updates itself as you grant permissions.",
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "Grant these in System Settings — this panel updates itself when you do.",
                )
                .weak(),
            );
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "macOS only applies a new Screen Recording grant after the app restarts.",
            )
            .weak(),
        );
        if ui.button("⟳  Restart CI-Applet now").clicked() {
            let ctx = ui.ctx().clone();
            self.request_restart(&ctx);
        }

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
            .or_else(|| {
                if self.has_credential() {
                    Some("API key".into())
                } else {
                    None
                }
            });
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

        // Verification: nothing above proves a byte reaches the server.
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Check it end to end").strong());
        ui.label(
            egui::RichText::new(
                "Takes one screenshot now and uploads it to your CI-Server — the same path normal \
                 capture uses. This sends a real picture of your screen.",
            )
            .weak(),
        );
        ui.add_space(6.0);
        let running = self.job_running();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !running && self.has_credential(),
                    egui::Button::new("📤  Send a test capture"),
                )
                .clicked()
            {
                self.start_test_capture();
            }
            if running {
                ui.spinner();
            }
        });
        ui.add_space(4.0);
        if let Some(result) = &self.capture_result {
            result_label(ui, result);
        } else {
            self.status_widget(ui);
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
                // Name the outcome honestly: finishing without a credential is
                // a real choice, and it stops the wizard auto-reopening.
                let label = if self.has_credential() {
                    "Finish"
                } else {
                    "Finish without connecting"
                };
                if ui.button(label).clicked() {
                    let ctx = ui.ctx().clone();
                    self.finish(&ctx);
                }
                if !self.has_credential() {
                    ui.label(
                        egui::RichText::new("Setup Assistant stays in the tray menu.").weak(),
                    );
                }
            });
        });
    }
}

/// Build a current-thread runtime and drive one future on it. Shared by every
/// background job so the runtime-construction failure is handled once.
fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => Ok(rt.block_on(fut)),
        Err(e) => Err(format!("runtime: {e}")),
    }
}

/// Render a job result as a green ✓ / red ✗ line.
fn result_label(ui: &mut egui::Ui, result: &JobResult) {
    match result {
        Ok(msg) => {
            ui.colored_label(Color32::from_rgb(0x2d, 0xd4, 0xb0), format!("✓ {msg}"));
        }
        Err(msg) => {
            ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), format!("✗ {msg}"));
        }
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
        self.poll_permissions(ctx);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app(config: AppConfig) -> OnboardingApp {
        OnboardingApp::new(
            Arc::new(ConfigManager::new().unwrap()),
            config,
            Arc::new(Mutex::new(false)),
            Arc::new(Mutex::new(false)),
            None,
        )
    }

    #[test]
    fn issuer_is_derived_from_the_server_url() {
        let mut config = AppConfig::default();
        config.server.portal_url = "https://ci.example.com".into();
        let a = app(config);
        assert!(!a.issuer_manual);
        assert_eq!(a.effective_issuer(), "https://ci.example.com/api/auth");
    }

    #[test]
    fn derived_issuer_tracks_a_retyped_server_url() {
        let mut a = app(AppConfig::default());
        a.server_url = "other.example.com".into();
        // No manual override, so the issuer follows the server URL.
        assert_eq!(a.effective_issuer(), "https://other.example.com/api/auth");
    }

    #[test]
    fn stored_split_deployment_issuer_is_preserved() {
        // Issuer that isn't <server>/api/auth = deliberate split deployment.
        // It must survive, and Advanced opens so it's visible.
        let mut config = AppConfig::default();
        config.server.portal_url = "https://ci.example.com".into();
        config.oidc.issuer = "https://hub.example.com/api/auth".into();
        let a = app(config);
        assert!(a.issuer_manual);
        assert!(a.show_advanced);
        assert_eq!(a.effective_issuer(), "https://hub.example.com/api/auth");
    }

    #[test]
    fn matching_stored_issuer_is_not_treated_as_manual() {
        let mut config = AppConfig::default();
        config.server.portal_url = "https://ci.example.com".into();
        config.oidc.issuer = "https://ci.example.com/api/auth".into();
        let a = app(config);
        assert!(!a.issuer_manual, "derived value should not pin Advanced open");
        assert!(!a.show_advanced);
    }

    #[test]
    fn falls_back_to_the_default_issuer_without_a_server_url() {
        let a = app(AppConfig::default());
        assert_eq!(a.effective_issuer(), DEFAULT_ISSUER);
    }
}
