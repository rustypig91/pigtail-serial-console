//! The update notice. Nothing here installs anything — "Download" opens the
//! release page in the browser.

use crate::app::App;

impl App {
    pub(crate) fn show_update_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.update_dialog else {
            return;
        };
        // Both this and `show_connect_error` anchor at CENTER_CENTER, so
        // showing them in the same frame would stack them exactly on top of
        // each other. Connect errors take priority; the update notice waits
        // its turn and reappears once the queue drains.
        if !self.connect_errors.is_empty() {
            return;
        }
        // Nothing to download or skip: this is a plain acknowledgement, same
        // shape as any other one-off failure dialog (e.g. `show_connect_error`).
        if dialog.download_url.is_none() && dialog.skip_version.is_none() {
            if show_ack_window(ctx, &dialog.title, &dialog.message) {
                self.update_dialog = None;
            }
            return;
        }
        // Decided inside the closure, acted on after it, so the handlers can
        // touch `self` (config, dialog state) without borrowing it twice.
        let mut open_url: Option<String> = None;
        let mut skip: Option<String> = None;
        let mut close = false;

        egui::Window::new(&dialog.title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(&dialog.message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if let Some(url) = &dialog.download_url {
                        if ui.button("Download").clicked() {
                            open_url = Some(url.clone());
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

        if let Some(url) = open_url {
            ctx.open_url(egui::OpenUrl::new_tab(url));
            close = true;
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
