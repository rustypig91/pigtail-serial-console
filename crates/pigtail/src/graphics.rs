//! Presentation settings and recovery after display changes.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eframe::egui_wgpu::{SurfaceErrorAction, WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu::{Backends, CurrentSurfaceTexture};

fn wgpu_setup(backend_override: Option<Backends>) -> WgpuSetup {
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    // Loading/enumerating Vulkan can crash inside older Intel Windows drivers,
    // before a usable adapter is selected. Exclude it from normal startup,
    // rather than just preferring DX12. Keep explicit WGPU_BACKEND overrides.
    let backends = backend_override.or(cfg!(target_os = "windows").then_some(Backends::DX12));
    if let Some(backends) = backends {
        setup.instance_descriptor.backends = backends;
    }
    setup.into()
}

/// Pace our animations without relying on the surface's presentation mode.
/// Input and reader wakes can still request an earlier frame.
pub fn request_animation_frame(context: &egui::Context) {
    request_repaint_after(context, Duration::from_millis(16));
}

/// Request a real delay, including for deadlines checked against `Instant`.
/// Avoid rendering repeatedly just before a deadline when vsync is disabled.
pub fn request_repaint_after(context: &egui::Context, delay: Duration) {
    if delay.is_zero() {
        context.request_repaint();
        return;
    }
    // egui subtracts predicted_dt from delayed repaint requests. Compensate
    // so short delays do not become an immediate busy loop.
    let predicted = context.input(|input| input.predicted_dt);
    context.request_repaint_after(
        delay.saturating_add(Duration::try_from_secs_f32(predicted).unwrap_or_default()),
    );
}

pub fn configuration(context: Arc<OnceLock<egui::Context>>) -> WgpuConfiguration {
    let on_surface_status = Arc::new(move |status: &CurrentSurfaceTexture| {
        if matches!(
            status,
            CurrentSurfaceTexture::Occluded | CurrentSurfaceTexture::Success(_)
        ) {
            return SurfaceErrorAction::SkipFrame;
        }
        tracing::warn!(?status, "rendering surface status");
        // Recovery skips this frame. An idle terminal otherwise has no reason
        // to repaint, so schedule a retry even without serial output or input.
        if !matches!(status, CurrentSurfaceTexture::Validation) {
            if let Some(context) = context.get() {
                context.request_repaint_after(Duration::from_millis(100));
            }
        }
        match status {
            CurrentSurfaceTexture::Lost => SurfaceErrorAction::RecreateSurface,
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Suboptimal(_) => {
                SurfaceErrorAction::Reconfigure
            }
            _ => SurfaceErrorAction::SkipFrame,
        }
    });
    WgpuConfiguration {
        wgpu_setup: wgpu_setup(Backends::from_env()),
        // FIFO presentation stalls on the affected Mesa/Wayland system. Let
        // wgpu select Immediate or Mailbox when available, with a safe fallback
        // on surfaces that only support FIFO. Forcing Mailbox would instead
        // fail startup on adapters without support for it.
        #[cfg(target_os = "linux")]
        surface: eframe::egui_wgpu::SurfaceConfig {
            present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
            ..eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY
        },
        on_surface_status,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextTestExt as _;
    use std::sync::Mutex;

    #[test]
    fn default_setup_excludes_vulkan_on_windows() {
        let WgpuSetup::CreateNew(setup) = wgpu_setup(None) else {
            panic!("startup must create a new graphics instance");
        };
        if cfg!(target_os = "windows") {
            // A preference/adapter selector alone still enumerates Vulkan.
            assert_eq!(setup.instance_descriptor.backends, Backends::DX12);
        } else {
            assert_eq!(
                setup.instance_descriptor.backends,
                WgpuSetupCreateNew::without_display_handle()
                    .instance_descriptor
                    .backends
            );
        }
    }

    #[test]
    fn explicit_backend_overrides_reach_the_instance_descriptor() {
        for backends in [
            Backends::DX12,
            Backends::VULKAN,
            Backends::GL | Backends::VULKAN,
        ] {
            let WgpuSetup::CreateNew(setup) = wgpu_setup(Some(backends)) else {
                panic!("startup must create a new graphics instance");
            };
            assert_eq!(setup.instance_descriptor.backends, backends);
        }
    }

    #[test]
    fn short_deadline_repaints_wait_and_due_deadlines_remain_immediate() {
        let context = egui::Context::default();
        for delay in [
            Duration::from_millis(1),
            Duration::from_secs(1),
            Duration::ZERO,
        ] {
            for _ in 0..3 {
                let _ = context.run_test_ui(Default::default(), |_| {});
            }
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            context.set_request_repaint_callback(move |request| {
                captured.lock().unwrap().push(request.delay);
            });
            request_repaint_after(&context, delay);
            assert_eq!(*requests.lock().unwrap(), [delay]);
        }
    }

    #[test]
    fn animation_repaints_are_delayed_but_do_not_postpone_input() {
        let context = egui::Context::default();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        context.set_request_repaint_callback(move |request| {
            captured.lock().unwrap().push(request.delay);
        });
        for _ in 0..3 {
            let _ = context.run_test_ui(Default::default(), |_| {});
        }
        requests.lock().unwrap().clear();
        request_animation_frame(&context);
        assert_eq!(*requests.lock().unwrap(), [Duration::from_millis(16)]);
        context.request_repaint();
        request_animation_frame(&context);
        assert_eq!(
            *requests.lock().unwrap(),
            [Duration::from_millis(16), Duration::ZERO]
        );
    }

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
            CurrentSurfaceTexture::Lost,
            CurrentSurfaceTexture::Outdated,
            CurrentSurfaceTexture::Timeout,
        ] {
            // Consume startup repaint requests, then leave the UI idle.
            for _ in 0..3 {
                let _ = context.run_test_ui(Default::default(), |_| {});
            }
            requests.lock().unwrap().clear();
            (configuration.on_surface_status)(&error);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            // egui subtracts its predicted frame time from the requested delay.
            assert!(requests[0] > Duration::ZERO);
            assert!(requests[0] <= Duration::from_millis(100));
        }

        let _ = context.run_test_ui(Default::default(), |_| {});
        requests.lock().unwrap().clear();
        (configuration.on_surface_status)(&CurrentSurfaceTexture::Validation);
        assert!(requests.lock().unwrap().is_empty());
    }
}
