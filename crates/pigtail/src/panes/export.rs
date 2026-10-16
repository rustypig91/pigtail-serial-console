//! Desktop progress presentation for the core export worker.

use crate::app::App;
use serialcore::export::{Snapshot, Task};
use std::path::{Path, PathBuf};
use std::time::Duration;

impl App {
    pub(crate) fn start_export(&mut self, snapshot: Snapshot, path: PathBuf, csv: bool) {
        if self.export.is_some() {
            return;
        }
        match Task::start(snapshot, path, csv, self.wake.clone()) {
            Ok(task) => self.export = Some(task),
            Err(error) => self.record_connect_error("Couldn't export", error.to_string()),
        }
    }

    pub(crate) fn poll_export(&mut self) {
        let Some(result) = self.export.as_ref().and_then(Task::poll) else {
            return;
        };
        self.export = None;
        if let Err(error) = result {
            self.record_connect_error("Couldn't export", error.to_string());
        }
    }

    pub(crate) fn show_export(&self, ctx: &egui::Context) {
        let Some(task) = &self.export else {
            return;
        };
        if show_progress(
            ctx,
            Progress {
                path: &task.path,
                completed: task.completed(),
                total: task.total,
                cancelling: task.cancelling(),
                can_cancel: task.can_cancel(),
            },
        )
        .clicked()
        {
            task.cancel();
        }
        // A bounded cadence covers progress and even an unexpected worker exit;
        // normal completion also wakes the app immediately.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

struct Progress<'a> {
    path: &'a Path,
    completed: usize,
    total: usize,
    cancelling: bool,
    can_cancel: bool,
}

fn show_progress(ctx: &egui::Context, progress: Progress<'_>) -> egui::Response {
    egui::Window::new("Exporting view")
        .collapsible(false)
        .resizable(false)
        .default_width(320.0)
        .default_pos(ctx.content_rect().center() - egui::vec2(160.0, 80.0))
        .frame(super::chrome::dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.add(egui::Label::new(progress.path.to_string_lossy()).wrap());
            let fraction = progress.completed as f32 / progress.total.max(1) as f32;
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(format!("{} / {} lines", progress.completed, progress.total)),
            );
            if progress.cancelling {
                ui.label("Cancelling…");
            } else if progress.completed == progress.total || !progress.can_cancel {
                ui.label("Finishing…");
            }
            ui.add_enabled(progress.can_cancel, egui::Button::new("Cancel"))
        })
        .expect("export window is always open")
        .inner
        .expect("export window cannot collapse")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;
    use crate::test_ui::ContextTestExt;

    #[test]
    fn progress_window_keeps_cancel_clickable_until_cancelling_or_commit() {
        let ctx = egui::Context::default();
        let mut button = None;
        let mut draw = |ui: &mut egui::Ui| {
            button = Some(show_progress(
                ui.ctx(),
                Progress {
                    path: Path::new("/tmp/captures/a-long-export-filename.csv"),
                    completed: 123,
                    total: 100_000,
                    cancelling: false,
                    can_cancel: true,
                },
            ));
        };
        ctx.run_test_ui(Default::default(), &mut draw);
        ctx.run_test_ui(Default::default(), &mut draw);
        let position = button.unwrap().rect.center();
        for pressed in [true, false] {
            ctx.run_test_ui(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ui| {
                    let response = show_progress(
                        ui.ctx(),
                        Progress {
                            path: Path::new("/tmp/captures/a-long-export-filename.csv"),
                            completed: 123,
                            total: 100_000,
                            cancelling: false,
                            can_cancel: true,
                        },
                    );
                    assert_eq!(response.clicked(), !pressed);
                },
            );
        }
        for cancelling in [true, false] {
            ctx.run_test_ui(Default::default(), |ui| {
                let response = show_progress(
                    ui.ctx(),
                    Progress {
                        path: Path::new("export.csv"),
                        completed: 100_000,
                        total: 100_000,
                        cancelling,
                        can_cancel: false,
                    },
                );
                assert!(!response.enabled());
            });
        }
    }

    #[test]
    fn worker_failure_uses_existing_error_dialog_and_allows_retry() {
        let (mut app, _tx) = test_app("export-worker-failure");
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.wake = serialcore::wake::Wake::new(move || tx.send(()).unwrap());
        app.start_export(
            Snapshot::new(None),
            app.paths.sessions.join("missing/export.txt"),
            false,
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        app.poll_export();
        assert!(app.export.is_none());
        assert_eq!(app.connect_errors.back().unwrap().title, "Couldn't export");
    }
}
