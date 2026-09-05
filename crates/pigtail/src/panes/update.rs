//! Update notices, download progress, installation, and retry.

use crate::app::App;

impl App {
    pub(crate) fn show_update_dialog(&mut self, ctx: &egui::Context) {
        // Do not stack two centred dialogs if a file was dropped while the
        // asynchronous update check was still in flight.
        if self.file_transfer_dialog.is_some() {
            return;
        }
        let Some(dialog) = &self.update_dialog else {
            return;
        };
        // Both this and `show_connect_error` anchor at CENTER_CENTER, so
        // showing them in the same frame would stack them exactly on top of
        // each other. Connect errors take priority; the update notice waits
        // its turn and reappears once the queue drains.
        if self.defer_to_connect_error() {
            return;
        }
        // Nothing to download or skip: this is a plain acknowledgement, same
        // shape as any other one-off failure dialog (e.g. `show_connect_error`).
        if dialog.update_version.is_none() && dialog.skip_version.is_none() {
            if show_ack_window(ctx, &dialog.title, &dialog.message) {
                self.update_dialog = None;
            }
            return;
        }
        // Decided inside the closure, acted on after it, so the handlers can
        // touch `self` (config, dialog state) without borrowing it twice.
        let mut install_version: Option<String> = None;
        let installing = self.install_rx.is_some();
        let checking = self.update_rx.is_some();
        let mut skip: Option<String> = None;
        let mut close = false;

        egui::Window::new(&dialog.title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(&dialog.message);
                ui.add_space(8.0);
                if installing {
                    if let Some(progress) = self.update_progress {
                        ui.add(egui::ProgressBar::new(progress).show_percentage());
                    } else {
                        ui.spinner();
                    }
                    return;
                }
                ui.horizontal(|ui| {
                    if let Some(version) = &dialog.update_version {
                        if ui
                            .add_enabled(!checking, egui::Button::new("Update"))
                            .clicked()
                        {
                            install_version = Some(version.clone());
                        }
                    }
                    if let Some(url) = &dialog.download_url {
                        if ui
                            .button("Downloads page")
                            .on_hover_text("Open this release in your browser to download manually")
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                    if let Some(version) = &dialog.skip_version {
                        if ui
                            .button("Skip this version")
                            .on_hover_text("Don't mention this release again at startup")
                            .clicked()
                        {
                            skip = Some(version.clone());
                        }
                    }
                    // The nothing-to-download case returns early above, so
                    // this is always the "download available" dialog.
                    if ui.button("Later").clicked() {
                        close = true;
                    }
                });
            });

        if let Some(version) = install_version {
            self.start_update_download(version);
        }
        if let Some(version) = skip {
            self.config.settings.skipped_version = Some(version);
            self.write_config();
            close = true;
        }
        if close {
            self.update_dialog = None;
        }
    }
}

/// A plain title+message+"Ok" acknowledgement window, shared by every dialog
/// that has nothing to offer beyond dismissal (the update notice's plain
/// case, and `show_connect_error`). Returns `true` once the user dismisses it.
pub(crate) fn show_ack_window(ctx: &egui::Context, title: &str, message: &str) -> bool {
    let mut close = false;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(message);
            ui.add_space(8.0);
            if ui.button("Ok").clicked() {
                close = true;
            }
        });
    close
}
