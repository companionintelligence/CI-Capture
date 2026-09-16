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
//!   Companion Memory server address is discovered rather than typed. Manual entry
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Connect,
    Permissions,
    Finish,
}

impl Step {
    /// Every step in order, including the one the wizard may skip. Navigation
    /// (`next`/`prev`) and the header chips must never iterate this directly —
    /// use [`Step::visible`], which drops `Permissions` where the OS does not
    /// gate capture. Drawing it from here put a phantom "Permission" chip in
    /// the header on Windows and Linux for a step that was never shown.
    const ALL: [Step; 4] = [
        Step::Welcome,
        Step::Connect,
        Step::Permissions,
        Step::Finish,
    ];

    /// The steps this wizard actually shows, in order: [`Step::ALL`] minus
    /// `Permissions` wherever [`permissions::permissions_relevant`] is false.
    /// Evaluated per call so the chip row, `next` and `prev` cannot disagree.
    fn visible() -> impl Iterator<Item = Step> {
        Self::ALL
            .into_iter()
            .filter(|s| *s != Step::Permissions || permissions::permissions_relevant())
    }

    fn short_label(self) -> String {
        match self {
            Step::Welcome => rust_i18n::t!("onboarding.step.welcome"),
            Step::Connect => rust_i18n::t!("onboarding.step.connect"),
            Step::Permissions => rust_i18n::t!("onboarding.step.permissions"),
            Step::Finish => rust_i18n::t!("onboarding.step.finish"),
        }
        .to_string()
    }

    fn title(self) -> String {
        match self {
            // The first screen's title is the bare product name, which is the
            // same in every locale — there is nothing to translate.
            Step::Welcome => "Companion Capture".to_string(),
            Step::Connect => rust_i18n::t!("onboarding.title.connect").to_string(),
            Step::Permissions => rust_i18n::t!("onboarding.title.permissions").to_string(),
            Step::Finish => rust_i18n::t!("onboarding.title.finish").to_string(),
        }
    }

    /// Position among the *visible* steps — what the header numbers chips by.
    /// A skipped `Permissions` step reports 0 rather than a slot the chip row
    /// does not have; it is never the current step on such a platform.
    fn index(self) -> usize {
        Self::visible().position(|s| s == self).unwrap_or(0)
    }

    /// The next visible step, if any.
    fn next(self) -> Option<Step> {
        Self::visible().skip_while(|s| *s != self).nth(1)
    }

    /// The previous visible step, if any.
    fn prev(self) -> Option<Step> {
        Self::visible().take_while(|s| *s != self).last()
    }
}

/// Where to get a Companion Memory server if you don't have one.
///
/// Companion Capture is a recorder — it needs somewhere to record *to*. A user
/// who installs this first and has no server has no way to guess that the answer
/// is "install Companion Hub", so say it, and make it one click.
const DOWNLOAD_URL: &str = "https://ci.computer/download";

fn no_server_hint(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        theme::caption(ui, &rust_i18n::t!("onboarding.no_server.prefix"));
        if ui
            .link(
                // "Companion Hub" is a product name, identical in every locale.
                egui::RichText::new("Companion Hub")
                    .font(theme::body(tokens::text::SMALL))
                    .color(theme::TEAL()),
            )
            .on_hover_text(DOWNLOAD_URL)
            .clicked()
        {
            let _ = crate::open_url(DOWNLOAD_URL);
        }
        theme::caption(ui, &rust_i18n::t!("onboarding.no_server.suffix"));
    });
}

/// Result of an async job (login or connection test): Ok(detail) / Err(msg).
type JobResult = Result<String, String>;

pub fn run_onboarding_ui() -> Result<()> {
    let manager = Arc::new(ConfigManager::new().context("building config manager")?);
    let config = manager.load_or_default().context("loading configuration")?;
    let completed = Arc::new(Mutex::new(false));

    let mut native_options = NativeOptions {
        centered: true,
        viewport: egui::ViewportBuilder::default()
            .with_title(rust_i18n::t!("onboarding.window.title").to_string())
            .with_inner_size([840.0, 680.0])
            .with_min_inner_size([720.0, 600.0])
            .with_active(true),
        ..Default::default()
    };
    crate::helper_window::configure_helper_window(&mut native_options);

    let manager_c = manager.clone();
    let completed_c = completed.clone();
    eframe::run_native(
        "Companion Capture Setup",
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
    /// (step, egui-monotonic start time) for the current step's entrance
    /// animation. `None` until the first frame, which is what makes the very
    /// first paint fade in too, not just step-to-step transitions.
    step_anim: Option<(Step, f64)>,

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

use crate::config::DEFAULT_PORTAL_ISSUER as DEFAULT_ISSUER;

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
            step_anim: None,
        }
    }

    /// 0 (just entered) → 1 (settled) for the current step, eased. Restarts
    /// whenever `self.step` changes; call once per frame, before drawing.
    fn step_progress(&mut self, ctx: &egui::Context) -> f32 {
        const ANIM_SECS: f64 = 0.3;
        let now = ctx.input(|i| i.time);
        let restart = !matches!(self.step_anim, Some((s, _)) if s == self.step);
        if restart {
            self.step_anim = Some((self.step, now));
        }
        let started = self.step_anim.expect("just set above if it was None").1;
        let t = ((now - started) / ANIM_SECS).clamp(0.0, 1.0) as f32;
        if t < 1.0 {
            ctx.request_repaint();
        }
        // Ease-out cubic: quick start, gentle settle — reads as "arriving",
        // not "sliding," which suits a step that replaces the one before it.
        1.0 - (1.0 - t).powi(3)
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
            Err(e) => {
                self.status_line = Some(Err(rust_i18n::t!(
                    "onboarding.status.save_failed",
                    error = e
                )
                .to_string()))
            }
        }
    }

    /// Canonicalize the typed address, reporting the problem in the wizard
    /// rather than writing an address that cannot work.
    ///
    /// This is a new user's first field, so it's the one most likely to arrive
    /// mistyped — and a bad value saved here surfaces much later as an opaque
    /// ingest error, with nothing pointing back at this screen.
    fn normalized_server_url(&mut self) -> Option<String> {
        let raw = self.server_url.trim();
        if raw.is_empty() {
            return Some(String::new());
        }
        match crate::server_common::normalize_server_url(raw) {
            Ok(url) => {
                // Show what was actually stored: the scheme it gained, or the
                // `/feed` it lost, should not be a silent edit.
                self.server_url = url.clone();
                Some(url)
            }
            Err(msg) => {
                self.status_line = Some(Err(msg));
                None
            }
        }
    }

    /// Persist the current Companion Memory server address + OIDC issuer (merge-safe).
    /// Returns false when the address was rejected, so callers can stop.
    fn save_connection_fields(&mut self) -> bool {
        let Some(url) = self.normalized_server_url() else {
            return false;
        };
        let issuer = self.oidc_issuer.trim().to_string();
        self.persist_merged(|c| {
            // Same rule as the settings form: an address typed here is the
            // user's choice and discovery must not overwrite it. An untouched
            // field leaves the provenance alone so discovery can still fill it.
            if !url.is_empty() && url != c.server.portal_url {
                c.server.url_source = crate::config::ServerUrlSource::Manual;
            }
            c.server.portal_url = url;
            c.oidc.issuer = issuer;
        });
        true
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
                rust_i18n::t!("onboarding.status.issuer_required").to_string()
            ));
            return;
        }
        if !self.save_connection_fields() {
            return;
        }
        let (tx, rx) = channel();
        self.login_rx = Some(rx);
        self.status_line = Some(Ok(
            rust_i18n::t!("onboarding.status.opening_browser").to_string()
        ));
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
            self.status_line = Some(Err(
                rust_i18n::t!("onboarding.status.api_key_required").to_string()
            ));
            return;
        }
        let Some(url) = self.normalized_server_url() else {
            return;
        };
        // Choosing an API key means API-key auth: store the key and clear any
        // stale OIDC session so the refresh loop doesn't fight it.
        self.persist_merged(|c| {
            c.server.portal_url = url;
            c.server.applet_auth_token = Some(key);
            c.oidc_session = None;
        });
        if self.has_credential() {
            self.status_line = Some(Ok(
                rust_i18n::t!("onboarding.status.api_key_saved").to_string()
            ));
            self.start_connection_test();
        }
    }

    fn start_connection_test(&mut self) {
        // A rejected address has nothing to test — the message from
        // `normalized_server_url` is the useful result here.
        if !self.save_connection_fields() {
            return;
        }
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
                    // This lands in `status_line`, i.e. on screen — not in the
                    // log — so it has to be a translated sentence.
                    let _ = tx.send(Err(rust_i18n::t!(
                        "onboarding.status.runtime_failed",
                        error = e
                    )
                    .to_string()));
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
            theme::logo(ui, &self.logo, 34.0, theme::TEAL());
            theme::heading(ui, title);
        });
        ui.add_space(14.0);

        // Named steps, not anonymous dots — few enough that there is no reason
        // to make the user guess how far along they are. Only the steps this
        // platform shows: a chip for a skipped step would count to four and
        // then jump straight past three.
        let here = self.step.index();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (i, label) in Step::visible().map(|s| s.short_label()).enumerate() {
                let (fill, fg) = match i.cmp(&here) {
                    std::cmp::Ordering::Less => (theme::tint(theme::TEAL(), 40), theme::TEAL()),
                    std::cmp::Ordering::Equal => (theme::TEAL(), theme::ON_TEAL()),
                    std::cmp::Ordering::Greater => (theme::CARD(), theme::FAINT()),
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
                Ok(m) => ("✓", theme::GREEN(), m),
                Err(m) => ("✗", theme::RED(), m),
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
        theme::subtitle(ui, &rust_i18n::t!("onboarding.welcome.subtitle"));
        ui.add_space(14.0);

        // The animated explainer: three lanes flowing to your Companion Memory.
        theme::capture_flow(ui, 150.0);
        ui.add_space(18.0);

        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            for (color, title, body) in [
                (
                    theme::LANE_SHOT(),
                    rust_i18n::t!("onboarding.welcome.lane.screens.title"),
                    rust_i18n::t!("onboarding.welcome.lane.screens.body"),
                ),
                (
                    theme::LANE_ACTIVITY(),
                    rust_i18n::t!("onboarding.welcome.lane.activity.title"),
                    rust_i18n::t!("onboarding.welcome.lane.activity.body"),
                ),
                (
                    theme::LANE_FILE(),
                    rust_i18n::t!("onboarding.welcome.lane.files.title"),
                    rust_i18n::t!("onboarding.welcome.lane.files.body"),
                ),
            ] {
                ui.horizontal(|ui| {
                    theme::status_dot(ui, color, false);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            egui::RichText::new(title.to_string())
                                .font(theme::semibold(tokens::text::LABEL))
                                .color(theme::TEXT()),
                        );
                        theme::caption(ui, &body);
                    });
                });
                ui.add_space(10.0);
            }
        });

        ui.add_space(14.0);
        theme::caption(ui, &rust_i18n::t!("onboarding.welcome.exclusions"));

        ui.add_space(10.0);
        no_server_hint(ui);
    }

    // ── step 2: connect, with everything automatic ──────────────────────────

    fn connect(&mut self, ui: &mut egui::Ui) {
        let running = self.job_running();

        theme::subtitle(ui, &rust_i18n::t!("onboarding.connect.subtitle"));
        ui.add_space(16.0);

        if self.has_credential() {
            theme::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::status_dot(ui, theme::GREEN(), true);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(
                            egui::RichText::new(self.who().unwrap_or_else(|| {
                                rust_i18n::t!("onboarding.connect.signed_in").to_string()
                            }))
                            .font(theme::semibold(tokens::text::BODY))
                            .color(theme::TEXT()),
                        );
                        let target = if self.server_url.trim().is_empty() {
                            rust_i18n::t!("onboarding.connect.discovering").to_string()
                        } else {
                            self.server_url.trim().to_string()
                        };
                        theme::mono_label(ui, &target, theme::MUTED());
                    });
                });
            });
            ui.add_space(12.0);
        } else {
            ui.add_enabled_ui(!running, |ui| {
                if theme::primary_button(ui, &rust_i18n::t!("onboarding.connect.portal_button"))
                    .clicked()
                {
                    self.start_login();
                }
            });
            ui.add_space(8.0);
            theme::caption(ui, &rust_i18n::t!("onboarding.connect.portal_help"));
            ui.add_space(14.0);
            no_server_hint(ui);
            ui.add_space(6.0);
        }

        if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(6.0);
                theme::caption(ui, &rust_i18n::t!("onboarding.connect.working"));
            });
            ui.add_space(10.0);
        }

        self.status_widget(ui);
        ui.add_space(12.0);

        // Everything below is the escape hatch, not the happy path. A fresh
        // install should never need to open it — which is why it is collapsed
        // and why nothing here is prefilled with someone else's hostname.
        egui::CollapsingHeader::new(
            egui::RichText::new(rust_i18n::t!("onboarding.connect.manual_header").to_string())
                .font(theme::body(tokens::text::SMALL))
                .color(theme::MUTED()),
        )
        .id_salt("onboarding_advanced")
        .show(ui, |ui| {
            ui.add_space(6.0);
            theme::field_label(ui, &rust_i18n::t!("onboarding.connect.server_label"));
            theme::text_field(
                ui,
                &mut self.server_url,
                &rust_i18n::t!("onboarding.connect.server_hint"),
                theme::FieldOpts::default(),
            );
            theme::caption(ui, &rust_i18n::t!("onboarding.connect.server_help"));
            ui.add_space(12.0);

            theme::field_label(ui, &rust_i18n::t!("onboarding.connect.issuer_label"));
            theme::text_field(
                ui,
                &mut self.oidc_issuer,
                DEFAULT_ISSUER,
                theme::FieldOpts::default(),
            );
            ui.add_space(12.0);

            theme::field_label(ui, &rust_i18n::t!("onboarding.connect.api_key_label"));
            ui.horizontal(|ui| {
                theme::text_field(
                    ui,
                    &mut self.api_key,
                    &rust_i18n::t!("onboarding.connect.api_key_hint"),
                    theme::FieldOpts {
                        password: true,
                        ..Default::default()
                    },
                );
                if theme::ghost_button(ui, &rust_i18n::t!("onboarding.connect.save_button"))
                    .clicked()
                {
                    self.start_api_key_save();
                }
            });
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !running && self.has_credential(),
                        egui::Button::new(
                            rust_i18n::t!("onboarding.connect.test_button").to_string(),
                        ),
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
        theme::subtitle(ui, &rust_i18n::t!("onboarding.permissions.subtitle"));
        ui.add_space(16.0);

        let sr = self.perms.screen_recording;
        let granted = sr.is_ok();

        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::status_dot(
                    ui,
                    if granted {
                        theme::GREEN()
                    } else {
                        theme::AMBER()
                    },
                    !granted,
                );
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(
                        egui::RichText::new(
                            rust_i18n::t!("onboarding.permissions.screen_recording").to_string(),
                        )
                        .font(theme::semibold(tokens::text::BODY))
                        .color(theme::TEXT()),
                    );
                    theme::caption(
                        ui,
                        &if granted {
                            rust_i18n::t!("onboarding.permissions.granted_detail")
                        } else {
                            rust_i18n::t!("onboarding.permissions.missing_detail")
                        },
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !granted {
                        if theme::primary_button(
                            ui,
                            &rust_i18n::t!("onboarding.permissions.allow_button"),
                        )
                        .clicked()
                        {
                            // Triggers the OS prompt on first call; afterwards
                            // macOS only honours the Settings pane.
                            permissions::request_screen_recording();
                            let _ = crate::open_url(permissions::screen_recording_settings_url());
                        }
                    } else {
                        theme::status_badge(
                            ui,
                            &rust_i18n::t!("onboarding.permissions.granted_badge"),
                            theme::GREEN(),
                        );
                    }
                });
            });
        });

        ui.add_space(14.0);
        if granted {
            ui.label(
                egui::RichText::new(format!(
                    "✓  {}",
                    rust_i18n::t!("onboarding.permissions.all_set")
                ))
                .font(theme::body(tokens::text::LABEL))
                .color(theme::GREEN()),
            );
        } else {
            theme::caption(ui, &rust_i18n::t!("onboarding.permissions.how_to"));
        }

        // Accessibility is deliberately absent. Nothing in the applet or in
        // vizier reads a single Accessibility API — window titles come from
        // CGWindowListCopyWindowInfo, which is gated by Screen Recording. Asking
        // for it pinned the tray to "⚠︎ Permissions needed" on working installs.
    }

    // ── step 4: what's on, and what to do next ──────────────────────────────

    fn finish_step(&mut self, ui: &mut egui::Ui) {
        theme::subtitle(ui, &rust_i18n::t!("onboarding.finish.subtitle"));
        ui.add_space(16.0);

        let connected = self.has_credential();
        theme::card(ui, |ui| {
            ui.set_width(ui.available_width());
            self.summary_row(
                ui,
                connected,
                &rust_i18n::t!("onboarding.finish.connected_title"),
                &match self.who() {
                    Some(w) if connected => format!("{w} · {}", self.server_url.trim()),
                    _ => rust_i18n::t!("onboarding.finish.not_connected").to_string(),
                },
            );
            ui.add_space(10.0);

            if permissions::permissions_relevant() {
                let sr = permissions::status().screen_recording;
                self.summary_row(
                    ui,
                    sr.is_ok(),
                    &rust_i18n::t!("onboarding.finish.screen_recording_title"),
                    &if sr.is_ok() {
                        rust_i18n::t!("onboarding.finish.screen_recording_ok")
                    } else {
                        rust_i18n::t!("onboarding.finish.screen_recording_missing")
                    },
                );
                ui.add_space(10.0);
            }

            let folders = self.config.file_tracker.watched_paths.len();
            self.summary_row(
                ui,
                folders > 0,
                &rust_i18n::t!("onboarding.finish.folders_title"),
                &match folders {
                    0 => rust_i18n::t!("onboarding.finish.folders_none").to_string(),
                    1 => rust_i18n::t!("onboarding.finish.folders_one").to_string(),
                    n => rust_i18n::t!("onboarding.finish.folders_many", count = n).to_string(),
                },
            );
        });

        ui.add_space(18.0);
        theme::field_label(ui, &rust_i18n::t!("onboarding.finish.where_label"));
        ui.add_space(8.0);
        // "Menu bar" is a Mac word; the catalogue carries tray wording for
        // Windows and Linux and `os_variant` picks the right one.
        theme::caption(
            ui,
            &crate::settings_ui::os_variant("onboarding.finish.where_body"),
        );
    }

    /// One ✓/• line in the finish summary.
    fn summary_row(&self, ui: &mut egui::Ui, ok: bool, title: &str, detail: &str) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(if ok { "✓" } else { "•" })
                    .font(theme::semibold(tokens::text::BODY))
                    .color(if ok { theme::GREEN() } else { theme::AMBER() }),
            );
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::body(tokens::text::LABEL))
                        .color(theme::TEXT()),
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
                    .then(|| rust_i18n::t!("onboarding.connect.signed_in_with_key").to_string())
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
                let back = format!("←  {}", rust_i18n::t!("onboarding.nav.back"));
                if theme::ghost_button(ui, &back).clicked() {
                    self.step = prev;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.step {
                    Step::Finish => {
                        if theme::primary_button(
                            ui,
                            &rust_i18n::t!("onboarding.nav.start_capturing"),
                        )
                        .clicked()
                        {
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
                            format!("{}  →", rust_i18n::t!("onboarding.nav.continue"))
                        } else {
                            format!("{}  →", rust_i18n::t!("onboarding.nav.skip"))
                        };
                        if theme::primary_button(ui, &label).clicked() {
                            // An empty address is a real skip and advances. One
                            // that was typed but can't be used holds the step,
                            // so the message lands next to the field that
                            // caused it rather than a screen later.
                            if self.save_connection_fields() {
                                self.perms = permissions::status();
                                if let Some(next) = self.step.next() {
                                    self.step = next;
                                }
                            }
                        }
                        if !self.has_credential() {
                            theme::caption(
                                ui,
                                &crate::settings_ui::os_variant("onboarding.nav.sign_in_later"),
                            );
                        }
                    }
                    _ => {
                        if let Some(next) = self.step.next() {
                            let label = if self.step == Step::Welcome {
                                format!("{}  →", rust_i18n::t!("onboarding.nav.get_started"))
                            } else {
                                format!("{}  →", rust_i18n::t!("onboarding.nav.continue"))
                            };
                            if theme::primary_button(ui, &label).clicked() {
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
        theme::sync(ctx);
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
                    .fill(theme::PANEL_HI())
                    .inner_margin(egui::Margin::symmetric(30, 16)),
            )
            .show(ctx, |ui| self.nav_bar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL())
                    .inner_margin(egui::Margin::symmetric(30, 26)),
            )
            .show(ctx, |ui| {
                let step = self.step;
                self.header(ui, &step.title());
                let eased = self.step_progress(ctx);
                // Every step scrolls. Without this the Connect step clipped
                // irrecoverably at the minimum window size, with no way to reach
                // the controls below the fold.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Fade + rise on entry, header/nav excluded so the step
                        // chips update instantly rather than fading with the body.
                        ui.add_space((1.0 - eased) * 12.0);
                        ui.scope(|ui| {
                            ui.set_opacity(eased);
                            match step {
                                Step::Welcome => self.welcome(ui),
                                Step::Connect => self.connect(ui),
                                Step::Permissions => self.permissions(ui),
                                Step::Finish => self.finish_step(ui),
                            }
                        });
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::Step;
    use crate::permissions;

    /// The header draws one chip per visible step. On macOS Screen Recording
    /// gates capture, so the Permission step — and its chip — is real; on
    /// Windows and Linux the wizard skips the step and must not draw a chip
    /// for it. This pins the visible list to `permissions_relevant()`, the
    /// same predicate `next`/`prev` skip on.
    #[test]
    fn visible_steps_include_permissions_only_where_the_os_gates_capture() {
        let visible: Vec<Step> = Step::visible().collect();
        assert_eq!(
            visible.contains(&Step::Permissions),
            permissions::permissions_relevant()
        );
        if cfg!(target_os = "macos") {
            assert_eq!(visible, Step::ALL.to_vec());
        } else {
            assert_eq!(visible, vec![Step::Welcome, Step::Connect, Step::Finish]);
        }
    }

    /// Walking `next` from the first step must visit exactly the visible
    /// steps in order and end, and `prev` must retrace it — the chip row,
    /// the "N of M" numbering and the navigation all read from one list.
    #[test]
    fn next_and_prev_walk_exactly_the_visible_steps() {
        let visible: Vec<Step> = Step::visible().collect();

        let mut forward = vec![visible[0]];
        while let Some(next) = forward.last().unwrap().next() {
            forward.push(next);
        }
        assert_eq!(forward, visible);

        let mut backward = vec![*visible.last().unwrap()];
        while let Some(prev) = backward.last().unwrap().prev() {
            backward.push(prev);
        }
        backward.reverse();
        assert_eq!(backward, visible);

        for (i, step) in visible.iter().enumerate() {
            assert_eq!(step.index(), i, "{i}: chip index must match visible order");
        }
    }
}
