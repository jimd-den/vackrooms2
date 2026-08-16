//! Canvas-2D blit driver for the CPU splatting renderer.
//!
//! All rendering logic lives in the platform-free
//! `adapters::cpu_splatter::SoftwareRasterizer`; this wrapper's only job is
//! to copy the finished RGBA framebuffer into the canvas via `ImageData` —
//! no WebGL context is created at all.

use wasm_bindgen::{Clamped, JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

use crate::adapters::cpu_splatter::SoftwareRasterizer;
use crate::application::ports::{ChunkDraw, FrameParams, RendererPort};
use crate::drivers::render_worker_pool::RenderWorkerPool;

pub struct CpuCanvasRenderer {
    rasterizer: SoftwareRasterizer,
    /// When present, bands are drawn by workers and this renderer only
    /// forwards atlas updates and frames. `None` is the single-threaded
    /// path, unchanged — and the path everything falls back to.
    pool: Option<RenderWorkerPool>,
    ctx: CanvasRenderingContext2d,
    settings: crate::adapters::cpu_splatter::CpuRenderSettings,
    /// Most recent synchronous CPU stages, measured independently so the
    /// HUD can distinguish renderer cost from Canvas-2D presentation cost.
    rasterizer_ms: f64,
    blit_ms: f64,
}

impl CpuCanvasRenderer {
    pub fn new(canvas: &HtmlCanvasElement) -> Result<Self, JsValue> {
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d context available"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let settings = crate::get_cpu_settings();
        // A pool that cannot be created is not an error: the single-threaded
        // renderer below is the same code, just all on one thread.
        let workers = crate::render_worker_count();
        let pool = (workers > 1)
            .then(|| {
                RenderWorkerPool::new(canvas, workers, canvas.width(), canvas.height()).ok()
            })
            .flatten();
        Ok(Self {
            rasterizer: SoftwareRasterizer::new(canvas.width() as usize, canvas.height() as usize),
            pool,
            ctx,
            settings,
            rasterizer_ms: 0.0,
            blit_ms: 0.0,
        })
    }

    /// True when bands are being drawn off the main thread.
    pub fn is_threaded(&self) -> bool {
        self.pool.is_some()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(pool) = self.pool.as_mut() {
            pool.resize(width.max(1), height.max(1));
        }
        // The browser composition root already applies the validated CPU
        // resolution factor to the canvas. Matching it exactly is essential:
        // `put_image_data` does not scale a smaller image and would otherwise
        // leave most of the canvas stale or black.
        self.rasterizer
            .resize(width.max(1) as usize, height.max(1) as usize);
    }

    pub fn telemetry_string(&self) -> String {
        let mut stats = self.rasterizer.telemetry();
        if let Some(pool) = self.pool.as_ref() {
            // The main-thread rasterizer drew nothing this frame, so its
            // counters would read zero. Report the workers' sums instead,
            // tagged with the band count so the HUD shows the pool is live.
            let [visited, splats, writes, depth, exhausted] = pool.telemetry_totals();
            stats.visited_nodes = visited as usize;
            stats.splat_count = splats as usize;
            stats.pixel_writes = writes as usize;
            stats.max_virtual_depth = depth as usize;
            stats.budget_exhausted = exhausted != 0;
            return format!(
                "CPU[x{}]:{:.1}ms V:{} (D:{})/S:{}/P:{}K{}",
                pool.band_count(),
                self.rasterizer_ms,
                stats.visited_nodes,
                stats.max_virtual_depth,
                stats.splat_count,
                stats.pixel_writes / 1000,
                if stats.budget_exhausted { "!" } else { "" }
            );
        }
        format!(
            "CPU:{:.1}+{:.1}ms V:{}/{}{} (D:{})/S:{}/P:{}K/{}K{}{}",
            self.rasterizer_ms,
            self.blit_ms,
            stats.visited_nodes,
            stats.node_visit_limit,
            if stats.node_budget_limited { "N!" } else { "" },
            stats.max_virtual_depth,
            stats.splat_count,
            stats.pixel_writes / 1000,
            stats.pixel_write_limit / 1000,
            if stats.pixel_budget_limited { "P!" } else { "" },
            if stats.budget_exhausted { "!" } else { "" }
        )
    }
}

impl RendererPort for CpuCanvasRenderer {
    fn upload_atlas(&mut self, texels: &[u32]) {
        // Both sides are kept in step: a worker that dies hands its band
        // back to this rasterizer, which must already hold the atlas.
        self.rasterizer.upload_atlas(texels);
        if let Some(pool) = self.pool.as_mut() {
            pool.upload_atlas(texels);
        }
    }

    fn upload_atlas_rows(&mut self, first_row: u32, texels: &[u32]) -> bool {
        let applied = self.rasterizer.upload_atlas_rows(first_row, texels);
        if let Some(pool) = self.pool.as_mut() {
            pool.upload_atlas_rows(first_row, texels);
        }
        applied
    }

    fn cpu_telemetry_string(&self) -> Option<String> {
        Some(self.telemetry_string())
    }

    fn draw(&mut self, frame: &FrameParams, chunks: &[ChunkDraw]) {
        self.settings = crate::get_cpu_settings();
        self.settings.fov_tan = crate::drivers::webgl::fov_tan();
        // The optimization switchboard is sampled once per frame here; the
        // platform-free rasterizer itself never reads globals.
        self.settings.toggles = crate::get_render_toggles();
        self.rasterizer.settings = self.settings;

        let render_start = browser_now_ms();
        if let Some(pool) = self.pool.as_mut() {
            pool.draw(frame, chunks, self.settings);
            if pool.all_failed() {
                // Every worker is gone. Drop to the single-threaded path
                // rather than presenting nothing.
                self.pool = None;
            } else {
                self.rasterizer_ms = browser_now_ms() - render_start;
                self.blit_ms = 0.0;
                return;
            }
        }
        self.rasterizer.draw(frame, chunks);
        let render_end = browser_now_ms();
        let width = self.rasterizer.width() as u32;
        let height = self.rasterizer.height() as u32;
        let fb = self.rasterizer.framebuffer();

        if let Ok(image) = ImageData::new_with_u8_clamped_array_and_sh(Clamped(fb), width, height) {
            let _ = self.ctx.put_image_data(&image, 0.0, 0.0);
        }
        let blit_end = browser_now_ms();
        self.rasterizer_ms = render_end - render_start;
        self.blit_ms = blit_end - render_end;
    }
}

fn browser_now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}
