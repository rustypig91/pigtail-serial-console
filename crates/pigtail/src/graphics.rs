//! Presentation settings and recovery after display changes.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eframe::egui_wgpu::{SurfaceErrorAction, WgpuConfiguration};
use eframe::wgpu::SurfaceError;

pub fn configuration(context: Arc<OnceLock<egui::Context>>) -> WgpuConfiguration {
    let on_surface_error = Arc::new(move |error| {
        tracing::warn!(%error, "rendering surface error");
        // Recovery skips this frame. An idle terminal otherwise has no reason
        // to repaint, so schedule a retry even without serial output or input.
        if error != SurfaceError::OutOfMemory {
            if let Some(context) = context.get() {
                context.request_repaint_after(Duration::from_millis(100));
            }
        }
        match error {
            SurfaceError::Lost | SurfaceError::Outdated => SurfaceErrorAction::RecreateSurface,
            SurfaceError::Timeout | SurfaceError::OutOfMemory => SurfaceErrorAction::SkipFrame,
        }
    });
    WgpuConfiguration {
        // FIFO presentation stalls on the affected Mesa/Wayland system. Let
        // wgpu select Immediate or Mailbox when available, with a safe fallback
        // on surfaces that only support FIFO. Forcing Mailbox would instead
        // fail startup on adapters without support for it.
        #[cfg(target_os = "linux")]
        present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
        on_surface_error,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn surface_recovery_wakes_an_idle_ui_without_serial_events() {
        let context = egui::Context::default();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        context.set_request_repaint_callback(move |request| {
            captured.lock().unwrap().push(request.delay);
        });
        let shared = Arc::new(OnceLock::new());
        shared.set(context.clone()).unwrap();
        let configuration = configuration(shared);

        for error in [
            SurfaceError::Lost,
            SurfaceError::Outdated,
            SurfaceError::Timeout,
        ] {
            // Consume startup repaint requests, then leave the UI idle.
            for _ in 0..3 {
                let _ = context.run(Default::default(), |_| {});
            }
            requests.lock().unwrap().clear();
            (configuration.on_surface_error)(error);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            // egui subtracts its predicted frame time from the requested delay.
            assert!(requests[0] > Duration::ZERO);
            assert!(requests[0] <= Duration::from_millis(100));
        }

        let _ = context.run(Default::default(), |_| {});
        requests.lock().unwrap().clear();
        (configuration.on_surface_error)(SurfaceError::OutOfMemory);
        assert!(requests.lock().unwrap().is_empty());
    }
}
