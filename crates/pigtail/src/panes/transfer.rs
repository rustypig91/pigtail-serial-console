//! Drop-to-send file confirmation, preparation, and progress controls.

use crate::app::{App, FileTransferDialog, TransferProgress};
use serialcore::config::LineEnding;
use serialcore::reader::ConnState;
use serialcore::store::PortId;
use serialcore::transfer::{self, TextDecoding, TransferMode, TransferOptions};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

const PREVIEW_BYTES: usize = 2048;

impl App {
    /// Consume native file-drop events only when there is an active console to
    /// receive them. Merely dropping opens a confirmation; it never transmits.
    pub(crate) fn poll_file_drop(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.first().cloned());
        let Some(dropped) = dropped else {
            return;
        };
        if self.floating_window_open() {
            return;
        }
        let Some(path) = dropped.path else {
            self.record_connect_error(
                "Couldn't open dropped file",
                "This build can only send files that have a local filesystem path.".to_owned(),
            );
            return;
        };
        self.open_file_transfer(path, "Couldn't open dropped file");
    }

    /// Open a native file picker for desktop sessions where the window backend
    /// cannot deliver drag-and-drop events (notably Wayland in winit 0.30).
    pub(crate) fn choose_file_transfer(&mut self) {
        if self.file_transfer_dialog.is_some() || self.any_file_transfer_active() {
            self.record_connect_error(
                "Couldn't start file transfer",
                "Finish or cancel the current file transfer first.".to_owned(),
            );
            return;
        }
        if self.file_transfer_target().is_none() {
            self.record_connect_error(
                "Couldn't start file transfer",
                "Connect the active console before choosing a file to send.".to_owned(),
            );
            return;
        }
        let Some(path) = rfd::FileDialog::new().pick_file() else {
            return;
        };
        self.open_file_transfer(path, "Couldn't open selected file");
    }

    fn open_file_transfer(&mut self, path: PathBuf, error_title: &'static str) {
        if self.file_transfer_dialog.is_some() || self.any_file_transfer_active() {
            self.record_connect_error(
                "Couldn't start file transfer",
                "Finish or cancel the current file transfer first.".to_owned(),
            );
            return;
        }
        let Some(port) = self.file_transfer_target() else {
            self.record_connect_error(
                "Couldn't start file transfer",
                "The active console is no longer connected.".to_owned(),
            );
            return;
        };
        let options = self
            .file_transfer_options
            .clone()
            .unwrap_or_else(|| TransferOptions {
                line_ending: self.connection_line_ending(port),
                ..Default::default()
            });
        match dropped_dialog(port, path, options) {
            Ok(dialog) => {
                self.file_transfer_dialog = Some(dialog);
                self.start_file_preparation();
            }
            Err(message) => self.record_connect_error(error_title, message),
        }
    }

    pub(crate) fn show_file_transfer(&mut self, ctx: &egui::Context) {
        self.poll_file_preparation();
        self.show_file_transfer_confirmation(ctx);
        self.show_file_transfer_progress(ctx);
    }

    /// Draw an unobtrusive target affordance while the OS says a file is over
    /// the app. The actual drop is still handled separately and requires the
    /// modal confirmation below.
    pub(crate) fn show_file_drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());
        if !hovering || self.file_transfer_target().is_none() || self.floating_window_open() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("file_drop_overlay"),
        ));
        let rect = ctx.screen_rect().shrink(28.0);
        painter.rect_filled(rect, 8.0, egui::Color32::from_black_alpha(190));
        painter.rect_stroke(
            rect,
            8.0,
            egui::Stroke::new(2.0_f32, egui::Color32::LIGHT_BLUE),
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop file to configure transfer",
            egui::FontId::proportional(24.0),
            egui::Color32::WHITE,
        );
        ctx.request_repaint();
    }

    fn file_transfer_target(&self) -> Option<PortId> {
        let conn = if self.merged_selected {
            let port = self.merged_tx_port?;
            self.connections.iter().find(|conn| conn.id == port)?
        } else {
            self.connections.get(self.active_index()?)?
        };
        (conn.state == ConnState::Connected).then_some(conn.id)
    }

    fn connection_line_ending(&self, port: PortId) -> LineEnding {
        self.connections
            .iter()
            .find(|conn| conn.id == port)
            .map_or(LineEnding::Lf, |conn| conn.port_config.line_ending)
    }

    fn any_file_transfer_active(&self) -> bool {
        self.connections
            .iter()
            .any(|conn| conn.transfer_progress.is_some())
    }

    fn start_file_preparation(&mut self) {
        let Some(dialog) = &mut self.file_transfer_dialog else {
            return;
        };
        dialog.prepared = None;
        dialog.prepare_error = None;
        let path = dialog.path.clone();
        let options = dialog.options.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let wake = self.wake.clone();
        match std::thread::Builder::new()
            .name("file-transfer-prepare".to_owned())
            .spawn(move || {
                let result = transfer::prepare(&path, &options);
                let _ = tx.send(result);
                wake.signal();
            }) {
            Ok(_) => dialog.prepare_rx = Some(rx),
            Err(error) => {
                dialog.prepare_rx = None;
                dialog.prepare_error = Some(format!("couldn't start file reader: {error}"));
            }
        }
    }

    fn poll_file_preparation(&mut self) {
        let Some(dialog) = &mut self.file_transfer_dialog else {
            return;
        };
        let Some(result) = dialog
            .prepare_rx
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        else {
            return;
        };
        dialog.prepare_rx = None;
        match result {
            Ok(prepared) => dialog.prepared = Some(prepared),
            Err(error) => dialog.prepare_error = Some(error),
        }
    }

    fn show_file_transfer_confirmation(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.file_transfer_dialog else {
            return;
        };
        let target = self
            .connections
            .iter()
            .find(|conn| conn.id == dialog.port)
            .map(|conn| {
                (
                    conn.display_label().to_owned(),
                    conn.state,
                    conn.port_config.baud,
                )
            });
        let mut cancel = false;
        let mut send = false;
        let mut options_changed = false;
        let mut line_delay_ms = dialog.options.line_delay.as_millis() as u64;
        let mut char_delay_ms = dialog.options.char_delay.as_millis() as u64;

        egui::Window::new("Send file")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("file_transfer_details")
                    .num_columns(2)
                    .spacing([16.0, 5.0])
                    .show(ui, |ui| {
                        ui.label("Device");
                        ui.label(target.as_ref().map_or("closed", |(name, _, _)| name));
                        ui.end_row();
                        ui.label("File");
                        ui.label(&dialog.file_name)
                            .on_hover_text(dialog.path.display().to_string());
                        ui.end_row();
                        ui.label("Source size");
                        ui.label(format_bytes(dialog.source_size));
                        ui.end_row();
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Send as");
                    egui::ComboBox::from_id_salt("file_transfer_mode")
                        .selected_text(dialog.options.mode.label())
                        .show_ui(ui, |ui| {
                            for mode in [TransferMode::Raw, TransferMode::Text, TransferMode::Hex] {
                                options_changed |= ui
                                    .selectable_value(&mut dialog.options.mode, mode, mode.label())
                                    .changed();
                            }
                        });
                });
                if dialog.options.mode == TransferMode::Text {
                    ui.horizontal(|ui| {
                        ui.label("Line ending");
                        egui::ComboBox::from_id_salt("file_transfer_line_ending")
                            .selected_text(dialog.options.line_ending.label())
                            .show_ui(ui, |ui| {
                                for ending in [
                                    LineEnding::None,
                                    LineEnding::Lf,
                                    LineEnding::CrLf,
                                    LineEnding::Cr,
                                ] {
                                    options_changed |= ui
                                        .selectable_value(
                                            &mut dialog.options.line_ending,
                                            ending,
                                            ending.label(),
                                        )
                                        .changed();
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Decode");
                        egui::ComboBox::from_id_salt("file_transfer_decoding")
                            .selected_text(dialog.options.text_decoding.label())
                            .show_ui(ui, |ui| {
                                for decoding in [
                                    TextDecoding::Utf8Strict,
                                    TextDecoding::Utf8Lossy,
                                    TextDecoding::Latin1,
                                ] {
                                    options_changed |= ui
                                        .selectable_value(
                                            &mut dialog.options.text_decoding,
                                            decoding,
                                            decoding.label(),
                                        )
                                        .changed();
                                }
                            });
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(dialog.options.mode != TransferMode::Raw, |ui| {
                        ui.label("Line delay");
                        options_changed |= ui
                            .add(
                                egui::DragValue::new(&mut line_delay_ms)
                                    .range(0..=60_000)
                                    .suffix(" ms"),
                            )
                            .changed();
                    });
                    ui.label("Character delay");
                    options_changed |= ui
                        .add(
                            egui::DragValue::new(&mut char_delay_ms)
                                .range(0..=10_000)
                                .suffix(" ms"),
                        )
                        .changed();
                });

                dialog.options.line_delay = Duration::from_millis(line_delay_ms);
                dialog.options.char_delay = Duration::from_millis(char_delay_ms);
                ui.separator();
                ui.label("Preview");
                egui::ScrollArea::vertical()
                    .max_height(110.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(preview_text(dialog)).monospace())
                                .wrap(),
                        );
                    });

                if let Some(error) = &dialog.prepare_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                } else if let Some(prepared) = &dialog.prepared {
                    let baud = target.as_ref().map_or(115_200, |(_, _, baud)| *baud).max(1);
                    let wire_time = Duration::from_secs_f64(
                        prepared.total_bytes() as f64 * 10.0 / f64::from(baud),
                    );
                    ui.label(format!(
                        "Will send {} · estimated duration {}",
                        format_bytes(prepared.total_bytes() as u64),
                        format_duration(prepared.estimated_duration().saturating_add(wire_time))
                    ));
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading and validating…");
                    });
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let connected = target
                        .as_ref()
                        .is_some_and(|(_, state, _)| *state == ConnState::Connected);
                    if ui
                        .add_enabled(
                            dialog.prepared.is_some() && connected,
                            egui::Button::new("Send"),
                        )
                        .clicked()
                    {
                        send = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if !connected {
                        ui.weak("Device disconnected; reconnect or cancel.");
                    }
                });
            });

        let remembered_options = options_changed.then(|| dialog.options.clone());
        if let Some(options) = remembered_options {
            self.file_transfer_options = Some(options);
            self.start_file_preparation();
        }
        if cancel {
            self.file_transfer_dialog = None;
        } else if send {
            self.begin_prepared_file_transfer();
        }
    }

    fn begin_prepared_file_transfer(&mut self) {
        let Some(mut dialog) = self.file_transfer_dialog.take() else {
            return;
        };
        let Some(prepared) = dialog.prepared.take() else {
            return;
        };
        let Some(conn) = self
            .connections
            .iter_mut()
            .find(|conn| conn.id == dialog.port && conn.state == ConnState::Connected)
        else {
            self.file_transfer_dialog = Some(dialog);
            return;
        };
        let total = prepared.total_bytes();
        conn.transfer_progress = Some(TransferProgress {
            file_name: dialog.file_name,
            sent: 0,
            total,
        });
        conn.handle.start_transfer(prepared);
    }

    fn show_file_transfer_progress(&mut self, ctx: &egui::Context) {
        let current = self.connections.iter().find_map(|conn| {
            conn.transfer_progress
                .as_ref()
                .map(|progress| (conn.id, conn.display_label().to_owned(), progress))
        });
        let Some((port, device, progress)) = current else {
            return;
        };
        let fraction = if progress.total == 0 {
            1.0
        } else {
            progress.sent as f32 / progress.total as f32
        };
        let text = format!(
            "{} / {}",
            format_bytes(progress.sent as u64),
            format_bytes(progress.total as u64)
        );
        let file_name = progress.file_name.clone();
        let mut cancel = false;
        egui::Window::new("Sending file")
            .id(egui::Id::new("file_transfer_progress"))
            .collapsible(false)
            .resizable(false)
            .default_pos(egui::pos2(20.0, 70.0))
            .show(ctx, |ui| {
                ui.label(format!("{file_name} → {device}"));
                ui.add(egui::ProgressBar::new(fraction).text(text));
                if ui.button("Cancel transfer").clicked() {
                    cancel = true;
                }
            });
        if cancel {
            if let Some(conn) = self.connections.iter().find(|conn| conn.id == port) {
                conn.handle.cancel_transfer();
            }
        }
    }
}

fn dropped_dialog(
    port: PortId,
    path: PathBuf,
    options: TransferOptions,
) -> Result<FileTransferDialog, String> {
    let metadata = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let source_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let mut preview = vec![0; PREVIEW_BYTES.min(source_len)];
    let read = file
        .read(&mut preview)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    preview.truncate(read);
    let file_name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok(FileTransferDialog {
        port,
        path,
        file_name,
        source_size: metadata.len(),
        preview,
        options,
        prepared: None,
        prepare_rx: None,
        prepare_error: None,
    })
}

fn preview_text(dialog: &FileTransferDialog) -> String {
    match dialog.options.mode {
        TransferMode::Text => String::from_utf8_lossy(&dialog.preview)
            .lines()
            .take(8)
            .collect::<Vec<_>>()
            .join("\n"),
        TransferMode::Hex => dialog
            .prepared
            .as_ref()
            .map(|prepared| hex_preview(&prepared.data))
            .unwrap_or_else(|| String::from_utf8_lossy(&dialog.preview).into_owned()),
        TransferMode::Raw => hex_preview(&dialog.preview),
    }
}

fn hex_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(64)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.is_zero() {
        "no added delay".to_owned()
    } else if duration.as_secs() < 60 {
        format!("{:.1} s", duration.as_secs_f64())
    } else {
        format!("{:.1} min", duration.as_secs_f64() / 60.0)
    }
}
