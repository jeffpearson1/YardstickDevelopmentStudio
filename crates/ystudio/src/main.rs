#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod elevation;
mod highlight;
mod soft;
mod theme;
mod views;
mod workflow;

use eframe::{egui, wgpu};

fn main() -> anyhow::Result<()> {
    // The hidden elevated worker for live install tests; see ystudio_core::elevation.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(ystudio_core::elevation::WORKER_ARG) {
        return ystudio_core::elevation::serve_from_args(&args[1..]);
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Yardstick Development Studio")
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([900.0, 600.0]);
    if let Some(icon) = theme::brand::window_icon() {
        viewport = viewport.with_icon(icon);
    }

    if let Some(reason) = software_reason() {
        return soft::run(viewport, &reason);
    }

    let mut options = eframe::NativeOptions { viewport, ..Default::default() };
    if let Ok(mode) = std::env::var("YSTUDIO_PRESENT") {
        options.wgpu_options.surface.present_mode = match mode.to_lowercase().as_str() {
            "fifo" => wgpu::PresentMode::Fifo,
            "immediate" => wgpu::PresentMode::Immediate,
            "mailbox" => wgpu::PresentMode::Mailbox,
            "novsync" => wgpu::PresentMode::AutoNoVsync,
            _ => wgpu::PresentMode::AutoVsync,
        };
    }
    if let Some(n) = std::env::var("YSTUDIO_FRAME_LATENCY").ok().and_then(|v| v.parse().ok()) {
        options.wgpu_options.surface.desired_maximum_frame_latency = Some(n);
    }
    eframe::run_native(
        "Yardstick Development Studio",
        options,
        Box::new(|cc| Ok(Box::new(app::StudioApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Why to paint on the CPU instead of through wgpu, if we should.
///
/// `YSTUDIO_RENDERER=software|wgpu` forces a choice. Otherwise the CPU
/// renderer is used when the only adapters are CPU ones (WARP on GPU-less
/// VMs and RDP sessions), because emulating the GPU is far slower than
/// rasterising egui's meshes directly.
fn software_reason() -> Option<String> {
    match std::env::var("YSTUDIO_RENDERER").unwrap_or_default().to_lowercase().as_str() {
        "software" | "cpu" => return Some("YSTUDIO_RENDERER".into()),
        "wgpu" | "gpu" => return None,
        _ => {}
    }
    let backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY);
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends, ..wgpu::InstanceDescriptor::new_without_display_handle() });
    let adapters = pollster::block_on(instance.enumerate_adapters(backends));
    let infos: Vec<_> = adapters.iter().map(wgpu::Adapter::get_info).collect();
    if infos.iter().any(|i| i.device_type != wgpu::DeviceType::Cpu) {
        return None;
    }
    Some(match infos.first() {
        Some(i) => format!("no GPU, only {}", i.name),
        None => "no GPU adapter".into(),
    })
}
