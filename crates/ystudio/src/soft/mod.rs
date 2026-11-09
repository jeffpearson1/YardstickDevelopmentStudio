//! A software-rendered runner for machines without a usable GPU.
//!
//! On VMs and RDP sessions without a GPU, wgpu falls back to the Microsoft
//! Basic Render Driver (WARP). WARP emulates a whole GPU pipeline on the CPU
//! and takes around half a second per frame. This runner keeps egui and winit
//! but skips the GPU: it rasterises egui's meshes on the CPU (see [`raster`])
//! and blits only the rows that changed to the window with softbuffer.

mod raster;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use eframe::egui::{self, ViewportId};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::app::StudioApp;

pub use raster::SoftRenderer;

struct RepaintAt(Instant);

struct Gfx {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    size: (u32, u32),
    state: egui_winit::State,
    info: egui::ViewportInfo,
}

struct Runner {
    ctx: egui::Context,
    viewport: egui::ViewportBuilder,
    renderer_label: String,
    gfx: Option<Gfx>,
    app: Option<StudioApp>,
    raster: SoftRenderer,
    repaint_at: Option<Instant>,
    /// Present every row next frame (after a resize or when uncovered).
    full_present: bool,
    error: Option<anyhow::Error>,
}

pub fn run(viewport: egui::ViewportBuilder, reason: &str) -> anyhow::Result<()> {
    let event_loop = EventLoop::<RepaintAt>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(true);
    ctx.set_request_repaint_callback(move |info| {
        let _ = proxy.send_event(RepaintAt(Instant::now() + info.delay));
    });
    let mut runner = Runner {
        ctx,
        viewport,
        renderer_label: format!("software ({reason}, {} threads)", rayon::current_num_threads()),
        gfx: None,
        app: None,
        raster: SoftRenderer::new(),
        repaint_at: None,
        full_present: true,
        error: None,
    };
    event_loop.run_app(&mut runner)?;
    match runner.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl Runner {
    fn create(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let window = Rc::new(egui_winit::create_window(&self.ctx, event_loop, &self.viewport)?);
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("softbuffer: {e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("softbuffer: {e}"))?;
        let state = egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(8192),
        );
        let mut info = egui::ViewportInfo::default();
        egui_winit::update_viewport_info(&mut info, &self.ctx, &window, true);
        window.request_redraw();
        self.gfx = Some(Gfx { window, surface, size: (0, 0), state, info });
        if self.app.is_none() {
            self.app = Some(StudioApp::with_context(&self.ctx, self.renderer_label.clone()));
        }
        Ok(())
    }

    fn paint(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let (Some(g), Some(app)) = (&mut self.gfx, &mut self.app) else { return Ok(()) };
        egui_winit::update_viewport_info(&mut g.info, &self.ctx, &g.window, false);
        let mut raw = g.state.take_egui_input(&g.window);
        raw.viewports = std::iter::once((ViewportId::ROOT, g.info.clone())).collect();
        let out = self.ctx.run_ui(raw, |ui| app.frame(ui));
        g.state.handle_platform_output_with_event_loop(&g.window, event_loop, out.platform_output);
        for (id, vo) in out.viewport_output {
            if id == ViewportId::ROOT {
                egui_winit::process_viewport_commands(&self.ctx, &mut g.info, vo.commands, &g.window, &mut Vec::new());
            }
        }

        let started = Instant::now();
        let mut textures = out.textures_delta;
        self.raster.set_textures(&textures);
        let size = g.window.inner_size();
        let result = match (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            (Some(w), Some(h)) => {
                let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
                let clear = self.ctx.global_style().visuals.panel_fill;
                let painted = self.raster.paint(w.get() as usize, h.get() as usize, out.pixels_per_point, clear, &prims);
                present(g, w, h, &self.raster, &painted.damage, std::mem::take(&mut self.full_present))
            }
            _ => Ok(()), // minimised
        };
        self.raster.free_textures(&textures);
        // Applied above; egui asserts (in debug builds) that handled deltas are cleared.
        textures.clear();
        app.perf.paint_ms = Some(started.elapsed().as_secs_f32() * 1000.0);
        result
    }
}

/// Copies the changed rows to the window and presents them.
fn present(g: &mut Gfx, w: NonZeroU32, h: NonZeroU32, raster: &SoftRenderer, damage: &[(usize, usize)], full: bool) -> anyhow::Result<()> {
    if damage.is_empty() && !full {
        return Ok(());
    }
    let err = |e: softbuffer::SoftBufferError| anyhow::anyhow!("softbuffer: {e}");
    if g.size != (w.get(), h.get()) {
        g.surface.resize(w, h).map_err(err)?;
        g.size = (w.get(), h.get());
    }
    let mut buf = g.surface.buffer_mut().map_err(err)?;
    let src = raster.pixels();
    let width = w.get() as usize;
    // Age 1 means the buffer still holds the last presented frame.
    if full || buf.age() != 1 {
        buf.copy_from_slice(src);
        buf.present().map_err(err)
    } else {
        let rects: Vec<softbuffer::Rect> = damage
            .iter()
            .map(|&(y0, y1)| {
                buf[y0 * width..y1 * width].copy_from_slice(&src[y0 * width..y1 * width]);
                softbuffer::Rect { x: 0, y: y0 as u32, width: w, height: NonZeroU32::new((y1 - y0) as u32).unwrap_or(NonZeroU32::MIN) }
            })
            .collect();
        buf.present_with_damage(&rects).map_err(err)
    }
}

impl ApplicationHandler<RepaintAt> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none()
            && let Err(e) = self.create(event_loop)
        {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, RepaintAt(at): RepaintAt) {
        self.repaint_at = Some(self.repaint_at.map_or(at, |r| r.min(at)));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.repaint_at {
            Some(at) if at <= Instant::now() => {
                self.repaint_at = None;
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                if let Some(app) = &mut self.app {
                    app.shutdown();
                }
                self.app = None;
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.paint(event_loop) {
                    self.error = Some(e);
                    event_loop.exit();
                }
                return;
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Occluded(false) => {
                self.full_present = true;
            }
            _ => {}
        }
        let Some(g) = &mut self.gfx else { return };
        if g.state.on_window_event(&g.window, &event).repaint || self.full_present {
            g.window.request_redraw();
        }
    }
}
