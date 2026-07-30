use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use eframe::{NativeOptions, egui};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::config::{AppConfig, ConfigManager};
use crate::theme;
use crate::server_upload::{
    ParseStatusValue, TerminalParseStatus, UploadedFile, poll_parse_status, supported_extensions,
    upload_files,
};

pub fn run_upload_ui() -> Result<()> {
    let manager = ConfigManager::new()?;
    let config = manager.load_or_default()?;

    let mut native_options = NativeOptions::default();
    native_options.viewport = egui::ViewportBuilder::default()
        .with_title("Upload Documents — CI-Applet")
        .with_inner_size([620.0, 520.0])
        .with_min_inner_size([480.0, 360.0]);

    eframe::run_native(
        "Upload Documents — CI-Applet",
        native_options,
        Box::new(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            crate::theme::apply_visuals(&cc.egui_ctx);
            let logo = crate::theme::load_logo(&cc.egui_ctx);
            Ok(Box::new(UploadApp::new(config, logo)))
        }),
    )
    .map_err(|err| anyhow!("failed to run upload UI: {err}"))
}

// ── Per-file state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Queued, not yet sent to server.
    Queued,
    /// Upload request in flight.
    Uploading,
    /// File existed on server already (deduplicated by hash), now polling.
    AlreadyUploaded { file_id: String },
    /// Fresh upload returned an ID; polling for parse status.
    Polling { file_id: String },
    /// Reached a terminal parse state.
    Done(DoneState),
    /// Network or HTTP error during upload (not a parse failure).
    UploadFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoneState {
    status: ParseStatusValue,
    error: Option<String>,
    parsed_at: Option<String>,
    was_duplicate: bool,
}

struct FileEntry {
    path: PathBuf,
    filename: String,
    phase: Phase,
}

// ── Channel messages from background tasks ────────────────────────────────────

enum StatusUpdate {
    UploadFailed { filename: String, error: String },
    Uploaded { filename: String, file: UploadedFile },
    PollDone { file_id: String, result: TerminalParseStatus, was_duplicate: bool },
}

// ── App ───────────────────────────────────────────────────────────────────────

struct UploadApp {
    config: Arc<AppConfig>,
    runtime: tokio::runtime::Runtime,
    files: Vec<FileEntry>,
    tx: UnboundedSender<StatusUpdate>,
    rx: UnboundedReceiver<StatusUpdate>,
    uploading: bool,
    upload_error: Option<String>,
    logo: egui::TextureHandle,
}

impl UploadApp {
    fn new(config: AppConfig, logo: egui::TextureHandle) -> Self {
        let (tx, rx) = unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create upload runtime");

        Self {
            config: Arc::new(config),
            runtime,
            files: Vec::new(),
            tx,
            rx,
            uploading: false,
            upload_error: None,
            logo,
        }
    }

    fn is_authenticated(&self) -> bool {
        self.config
            .server
            .applet_auth_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
    }

    fn pick_files(&mut self) {
        let patterns: Vec<String> = supported_extensions()
            .iter()
            .map(|ext| format!("*.{ext}"))
            .collect();
        let pattern_refs: Vec<&str> = patterns.iter().map(String::as_str).collect();

        let paths = tinyfiledialogs::open_file_dialog_multi(
            "Select Documents",
            "",
            Some((pattern_refs.as_slice(), "Supported Documents")),
        );

        if let Some(paths) = paths {
            for path_str in paths {
                let path = PathBuf::from(path_str);
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                // Don't add duplicates already in the list.
                if !self.files.iter().any(|f| f.path == path) {
                    self.files.push(FileEntry {
                        path,
                        filename,
                        phase: Phase::Queued,
                    });
                }
            }
        }
    }

    fn start_upload(&mut self, ctx: &egui::Context) {
        let queued_paths: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|f| f.phase == Phase::Queued)
            .map(|f| f.path.clone())
            .collect();

        if queued_paths.is_empty() {
            return;
        }

        self.uploading = true;
        self.upload_error = None;

        // Mark all queued entries as uploading.
        for entry in self.files.iter_mut() {
            if entry.phase == Phase::Queued {
                entry.phase = Phase::Uploading;
            }
        }

        let config = self.config.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();

        self.runtime.spawn(async move {
            match upload_files(&config, &queued_paths).await {
                Err(err) => {
                    // Treat as a whole-batch failure — mark every file.
                    for path in &queued_paths {
                        let filename = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let _ = tx.send(StatusUpdate::UploadFailed {
                            filename,
                            error: err.to_string(),
                        });
                    }
                    ctx.request_repaint();
                }
                Ok(uploaded) => {
                    for file in uploaded {
                        let already_existed = file.already_existed;
                        let file_id = file.id.clone();
                        let _ = tx.send(StatusUpdate::Uploaded {
                            filename: file.filename.clone(),
                            file,
                        });
                        ctx.request_repaint();

                        // Spawn a poll task per file.
                        let config = config.clone();
                        let tx = tx.clone();
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            let result =
                                poll_parse_status(&config, &file_id).await.unwrap_or_else(|err| {
                                    log::error!("poll error for {file_id}: {err:?}");
                                    TerminalParseStatus {
                                        value: ParseStatusValue::Skipped,
                                        error: None,
                                        parsed_at: None,
                                    }
                                });
                            let _ = tx.send(StatusUpdate::PollDone {
                                file_id,
                                result,
                                was_duplicate: already_existed,
                            });
                            ctx.request_repaint();
                        });
                    }
                }
            }
        });
    }

    fn drain_updates(&mut self) {
        while let Ok(update) = self.rx.try_recv() {
            match update {
                StatusUpdate::UploadFailed { filename, error } => {
                    if let Some(entry) = self.files.iter_mut().find(|f| f.filename == filename) {
                        entry.phase = Phase::UploadFailed(error);
                    }
                    self.uploading = false;
                }
                StatusUpdate::Uploaded { filename, file } => {
                    if let Some(entry) = self.files.iter_mut().find(|f| f.filename == filename) {
                        entry.phase = if file.already_existed {
                            Phase::AlreadyUploaded { file_id: file.id }
                        } else {
                            Phase::Polling { file_id: file.id }
                        };
                    }
                }
                StatusUpdate::PollDone { file_id, result, was_duplicate } => {
                    for entry in self.files.iter_mut() {
                        if matches!(&entry.phase,
                            Phase::Polling { file_id: id } | Phase::AlreadyUploaded { file_id: id }
                            if id == &file_id)
                        {
                            entry.phase = Phase::Done(DoneState {
                                status: result.value,
                                error: result.error,
                                parsed_at: result.parsed_at,
                                was_duplicate,
                            });
                            break;
                        }
                    }
                    // If no files are still in-flight, unlock the upload button.
                    let any_in_flight = self.files.iter().any(|f| {
                        matches!(f.phase, Phase::Uploading | Phase::Polling { .. } | Phase::AlreadyUploaded { .. })
                    });

                    if !any_in_flight {
                        self.uploading = false;
                    }
                }
            }
        }
    }
}

impl eframe::App for UploadApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_updates();

        let authed = self.is_authenticated();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(22, 20)),
            )
            .show(ctx, |ui| {
                // Header: logo + title.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    theme::logo(ui, &self.logo, 28.0, egui::Color32::WHITE);
                    ui.label(
                        egui::RichText::new("Upload Documents")
                            .font(theme::light(18.0))
                            .color(theme::TEXT),
                    );
                });
                ui.add_space(16.0);

                // Auth status + action buttons.
                ui.horizontal(|ui| {
                    if authed {
                        theme::status_dot(ui, theme::GREEN, false);
                        ui.label(
                            egui::RichText::new("Signed in")
                                .font(theme::body(12.0))
                                .color(theme::MUTED),
                        );
                    } else {
                        theme::status_dot(ui, theme::AMBER, false);
                        ui.label(
                            egui::RichText::new("Not logged in — use the tray 'Login to …' first")
                                .font(theme::body(12.0))
                                .color(theme::AMBER),
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 9.0;
                        let can_upload = authed
                            && !self.uploading
                            && self.files.iter().any(|f| f.phase == Phase::Queued);
                        let upload = ui
                            .add_enabled_ui(can_upload, |ui| theme::primary_button(ui, "Upload"))
                            .inner
                            .on_disabled_hover_text("Log in first via the tray menu 'Login to …'.");
                        if upload.clicked() {
                            self.start_upload(ctx);
                        }
                        if theme::ghost_button(ui, "Add Files…").clicked() {
                            self.pick_files();
                        }
                    });
                });

                if let Some(err) = self.upload_error.clone() {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(err)
                            .font(theme::body(11.5))
                            .color(theme::RED),
                    );
                }

                ui.add_space(16.0);
                dropzone(ui);
                ui.add_space(16.0);

                if self.files.is_empty() {
                    theme::card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical_centered(|ui| {
                            ui.add_space(8.0);
                            theme::caption(ui, "No files added yet.");
                            ui.add_space(8.0);
                        });
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (i, entry) in self.files.iter().enumerate() {
                                if i > 0 {
                                    ui.add_space(8.0);
                                }
                                file_row(ui, entry);
                            }
                        });
                }
            });
    }
}

/// File extension (lowercase, no dot) of a filename.
fn ext_of(filename: &str) -> &str {
    filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("")
}

/// The dashed "drop documents" target.
fn dropzone(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::new(ui.available_width(), 84.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(13), theme::tint(egui::Color32::BLACK, 30));
    // Dashed rounded-ish border (four dashed edges).
    let inset = rect.shrink(1.0);
    let corners = [
        inset.left_top(),
        inset.right_top(),
        inset.right_bottom(),
        inset.left_bottom(),
        inset.left_top(),
    ];
    let stroke = egui::Stroke::new(1.5, theme::tint(egui::Color32::WHITE, 40));
    for pair in corners.windows(2) {
        for dash in egui::Shape::dashed_line(pair, stroke, 6.0, 5.0) {
            painter.add(dash);
        }
    }
    painter.text(
        egui::pos2(rect.center().x, rect.center().y - 9.0),
        egui::Align2::CENTER_CENTER,
        "Drop documents to upload",
        theme::body(13.0),
        theme::MUTED,
    );
    let exts = supported_extensions()
        .iter()
        .map(|e| e.to_uppercase())
        .collect::<Vec<_>>()
        .join(" · ");
    painter.text(
        egui::pos2(rect.center().x, rect.center().y + 11.0),
        egui::Align2::CENTER_CENTER,
        exts,
        theme::mono(10.5),
        theme::FAINT,
    );
}

/// A single file entry: type badge, filename, trailing status pill.
fn file_row(ui: &mut egui::Ui, entry: &FileEntry) {
    egui::Frame::new()
        .fill(theme::tint(egui::Color32::BLACK, 40))
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                theme::file_badge(ui, ext_of(&entry.filename));
                ui.label(
                    egui::RichText::new(&entry.filename)
                        .font(theme::body(13.0))
                        .color(theme::TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    render_phase(ui, &entry.phase);
                });
            });
        });
}

fn render_phase(ui: &mut egui::Ui, phase: &Phase) {
    match phase {
        Phase::Queued => {
            theme::status_badge(ui, "Queued", theme::MUTED);
        }
        Phase::Uploading => {
            theme::status_badge(ui, "Uploading", theme::AMBER);
        }
        Phase::Polling { .. } | Phase::AlreadyUploaded { .. } => {
            theme::status_badge(ui, "Processing", theme::AMBER);
        }
        Phase::Done(state) => match state.status {
            ParseStatusValue::Completed => {
                theme::status_badge(ui, "Ready", theme::GREEN);
            }
            ParseStatusValue::Failed => {
                let resp = theme::status_badge(ui, "Parse failed", theme::RED);
                if let Some(err) = &state.error {
                    resp.on_hover_text(err);
                }
            }
            ParseStatusValue::Skipped => {
                if state.was_duplicate {
                    theme::status_badge(ui, "Already uploaded", theme::MUTED).on_hover_text(
                        "This file already exists on the server. Parsing may take a few minutes for larger documents.",
                    );
                } else {
                    theme::status_badge(ui, "Uploaded · not parsed", theme::MUTED).on_hover_text(
                        "File type is not parsed, but the file was uploaded successfully.",
                    );
                }
            }
        },
        Phase::UploadFailed(err) => {
            theme::status_badge(ui, "Upload failed", theme::RED).on_hover_text(err);
        }
    }
}
