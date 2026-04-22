use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use eframe::{NativeOptions, egui};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::config::{AppConfig, ConfigManager};
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
        .with_inner_size([560.0, 420.0])
        .with_min_inner_size([420.0, 300.0]);

    eframe::run_native(
        "Upload Documents — CI-Applet",
        native_options,
        Box::new(move |_cc| Ok(Box::new(UploadApp::new(config)))),
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
}

impl UploadApp {
    fn new(config: AppConfig) -> Self {
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

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Upload Documents");
            ui.add_space(4.0);

            if !self.is_authenticated() {
                ui.colored_label(
                    egui::Color32::from_rgb(208, 17, 54),
                    "Not logged in. Use the tray menu 'Login to …' first.",
                );
                ui.add_space(8.0);
            }

            ui.horizontal(|ui| {
                if ui.button("Add Files…").clicked() {
                    self.pick_files();
                }

                let can_upload = self.is_authenticated()
                    && !self.uploading
                    && self.files.iter().any(|f| f.phase == Phase::Queued);

                let upload_btn = ui
                    .add_enabled(can_upload, egui::Button::new("Upload"))
                    .on_disabled_hover_text("Log in first via the tray menu 'Login to …'.");
                if upload_btn.clicked() {
                    self.start_upload(ctx);
                }
            });

            if let Some(err) = &self.upload_error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(208, 17, 54), err);
            }

            ui.add_space(8.0);
            ui.separator();

            if self.files.is_empty() {
                ui.add_space(16.0);
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("No files added yet.").italics());
                });
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for entry in &self.files {
                        ui.horizontal(|ui| {
                            ui.label(&entry.filename);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                render_phase(ui, &entry.phase);
                            });
                        });
                        ui.separator();
                    }
                });
            }
        });
    }
}

fn render_phase(ui: &mut egui::Ui, phase: &Phase) {
    match phase {
        Phase::Queued => {
            ui.label(egui::RichText::new("Ready to upload").color(egui::Color32::GRAY));
        }
        Phase::Uploading => {
            ui.label(egui::RichText::new("Uploading…").color(egui::Color32::YELLOW));
        }
        Phase::Polling { .. } | Phase::AlreadyUploaded { .. } => {
            ui.label(egui::RichText::new("Processing…").color(egui::Color32::YELLOW));
        }
        Phase::Done(state) => match state.status {
            ParseStatusValue::Completed => {
                ui.label(egui::RichText::new("Ready").color(egui::Color32::GREEN));
            }
            ParseStatusValue::Failed => {
                let label = egui::RichText::new("Parse failed").color(egui::Color32::RED);
                let response = ui.label(label);
                if let Some(err) = &state.error {
                    response.on_hover_text(err);
                }
            }
            ParseStatusValue::Skipped => {
                if state.was_duplicate {
                    ui.label(egui::RichText::new("Already uploaded").color(egui::Color32::GRAY))
                        .on_hover_text(
                            "This file already exists on the server. Parsing may take a few minutes for larger documents.",
                        );
                } else {
                    ui.label(egui::RichText::new("Uploaded").color(egui::Color32::GRAY))
                        .on_hover_text("File type is not parsed, but the file was uploaded successfully.");
                }
            }
        },
        Phase::UploadFailed(err) => {
            ui.label(egui::RichText::new("Upload failed").color(egui::Color32::RED))
                .on_hover_text(err);
        }
    }
}
