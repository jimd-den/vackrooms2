//! A pool of render workers, one per horizontal band of the framebuffer.
//!
//! This is the threading tier that works everywhere. It needs no
//! cross-origin isolation and no `SharedArrayBuffer`, so it runs in base
//! Firefox on Linux — where WebGPU does not exist at all — as well as
//! anywhere else. Each worker owns its own wasm instance, its own atlas
//! copy, and one band of the frame; nothing is shared, so nothing needs
//! locking.
//!
//! The scheduling rules live in [`RenderBandScheduler`], which is
//! platform-free and unit-tested. This module owns only the parts that
//! cannot be: spawning `Worker`s, moving bytes and bitmaps across
//! `postMessage`, and compositing onto the visible canvas.
//!
//! Degradation is deliberate and never lands on untested code. A band whose
//! worker dies is drawn by the main thread using the same band-assigned
//! rasterizer the worker was running; if every worker dies, the caller drops
//! to the ordinary single-threaded path. Each rung already exists.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Reflect, Uint8Array, Uint32Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, ImageBitmap, MessageEvent, Worker, WorkerOptions,
    WorkerType,
};

use crate::adapters::cpu_splatter::SoftwareRasterizer;
use crate::adapters::render_band_scheduler::RenderBandScheduler;
use crate::adapters::render_frame_codec::{RenderFrameRequest, encode_render_frame};
use crate::application::ports::{ChunkDraw, FrameParams, RendererPort};

/// Upper bound on render workers.
///
/// Each one holds its own copy of the atlas and its MIP pyramid — tens of
/// megabytes at a full streaming radius — so this is a memory ceiling as
/// much as a parallelism one. Four captures most of the win on a typical
/// machine without putting a phone under memory pressure.
pub const MAX_RENDER_WORKERS: usize = 4;

/// A band bitmap waiting for its siblings, so the frame can be shown whole.
struct PendingBand {
    frame_id: u32,
    row_offset: u32,
    bitmap: ImageBitmap,
}

/// Shared between the pool and its message handlers.
struct PoolState {
    scheduler: RenderBandScheduler,
    pending: Vec<Option<PendingBand>>,
    /// Latest telemetry per band, summed for the HUD.
    telemetry: Vec<[u32; 5]>,
    /// Bands whose worker asked for a full atlas resend after rejecting an
    /// incremental block.
    atlas_resend: Vec<bool>,
}

pub struct RenderWorkerPool {
    /// Indexed by band, so a band that failed to spawn leaves a hole rather
    /// than shifting every later worker onto the wrong band.
    workers: Vec<Option<Worker>>,
    state: Rc<RefCell<PoolState>>,
    context: CanvasRenderingContext2d,
    /// Kept alive for the workers' lifetime; dropping these detaches the
    /// handlers and the pool goes deaf.
    _message_handlers: Vec<Closure<dyn FnMut(MessageEvent)>>,
    _error_handlers: Vec<Closure<dyn FnMut(web_sys::Event)>>,
    /// Draws bands the workers cannot. Built only if one actually fails.
    fallback: Option<SoftwareRasterizer>,
    width: u32,
    height: u32,
    /// Bumped whenever the draw table changes, so steady-state frames can
    /// omit it. Workers cache the table under this version.
    chunks_version: u32,
    last_chunks: Vec<ChunkDraw>,
    /// Whole-atlas copy, retained so a worker that rejects an incremental
    /// block can be resynced without waiting for the streamer.
    atlas: Vec<u32>,
}

impl RenderWorkerPool {
    /// Spawns `band_count` workers, one per band.
    ///
    /// Fails only if no worker can be created at all; the caller then uses
    /// the single-threaded renderer. A pool that comes up partially is a
    /// success — the missing bands fall to the main thread.
    pub fn new(
        canvas: &HtmlCanvasElement,
        band_count: usize,
        width: u32,
        height: u32,
    ) -> Result<Self, JsValue> {
        let band_count = band_count.clamp(1, MAX_RENDER_WORKERS);
        let context = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d context for render worker compositing"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        // The single-file bundle has no sibling script on disk; it publishes
        // a blob: URL on the global scope instead, exactly as the generation
        // worker does.
        let script_url = Reflect::get(
            &js_sys::global(),
            &"__VACKROOMS_RENDER_WORKER_URL__".into(),
        )
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| "render_worker.js".to_string());

        let state = Rc::new(RefCell::new(PoolState {
            scheduler: RenderBandScheduler::new(band_count),
            pending: (0..band_count).map(|_| None).collect(),
            telemetry: vec![[0; 5]; band_count],
            atlas_resend: vec![false; band_count],
        }));

        let mut workers = Vec::with_capacity(band_count);
        let mut message_handlers = Vec::with_capacity(band_count);
        let mut error_handlers = Vec::with_capacity(band_count);
        let mut spawned_any = false;

        for band_index in 0..band_count {
            let options = WorkerOptions::new();
            options.set_type(WorkerType::Module);
            // Relative, so a project-page subpath deploy still resolves it.
            let worker = match Worker::new_with_options(&script_url, &options) {
                Ok(worker) => worker,
                Err(_) => {
                    // This band is the main thread's problem now.
                    state.borrow_mut().scheduler.mark_failed(band_index);
                    workers.push(None);
                    continue;
                }
            };

            let message_state = state.clone();
            let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                handle_worker_message(&message_state, band_index, &event);
            });
            worker.set_onmessage(Some(handler.as_ref().unchecked_ref()));

            let error_state = state.clone();
            let worker_for_error = worker.clone();
            let error_handler = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event| {
                error_state.borrow_mut().scheduler.mark_failed(band_index);
                worker_for_error.terminate();
            });
            worker.set_onerror(Some(error_handler.as_ref().unchecked_ref()));

            let init = js_sys::Object::new();
            Reflect::set(&init, &"type".into(), &"init".into())?;
            Reflect::set(&init, &"bandIndex".into(), &(band_index as u32).into())?;
            Reflect::set(&init, &"bandCount".into(), &(band_count as u32).into())?;
            Reflect::set(&init, &"width".into(), &width.into())?;
            Reflect::set(&init, &"height".into(), &height.into())?;
            if worker.post_message(&init).is_err() {
                state.borrow_mut().scheduler.mark_failed(band_index);
                worker.terminate();
                workers.push(None);
                continue;
            }

            spawned_any = true;
            workers.push(Some(worker));
            message_handlers.push(handler);
            error_handlers.push(error_handler);
        }

        if !spawned_any {
            return Err(JsValue::from_str("no render workers could be spawned"));
        }

        Ok(Self {
            workers,
            state,
            context,
            _message_handlers: message_handlers,
            _error_handlers: error_handlers,
            fallback: None,
            width,
            height,
            chunks_version: 0,
            last_chunks: Vec::new(),
            atlas: Vec::new(),
        })
    }

    pub fn band_count(&self) -> usize {
        self.state.borrow().scheduler.band_count()
    }

    /// True once every worker has died and the caller should stop using the
    /// pool entirely.
    pub fn all_failed(&self) -> bool {
        self.state.borrow().scheduler.all_failed()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        if let Some(fallback) = self.fallback.as_mut() {
            fallback.resize(width.max(1) as usize, height.max(1) as usize);
        }
        // Workers re-derive their band from the next frame's dimensions, so
        // no separate resize message is needed.
    }

    pub fn upload_atlas(&mut self, texels: &[u32]) {
        self.atlas = texels.to_vec();
        for band in 0..self.workers.len() {
            self.post_full_atlas(band);
        }
        if let Some(fallback) = self.fallback.as_mut() {
            fallback.upload_atlas(texels);
        }
    }

    pub fn upload_atlas_rows(&mut self, first_row: u32, texels: &[u32]) -> bool {
        // Mirror into the retained copy so a resend stays correct.
        let start = first_row as usize * crate::application::atlas::ROW_TEXELS * 4;
        if start + texels.len() <= self.atlas.len() {
            self.atlas[start..start + texels.len()].copy_from_slice(texels);
        }

        let message = js_sys::Object::new();
        let ok = Reflect::set(&message, &"type".into(), &"atlasRows".into()).is_ok()
            && Reflect::set(&message, &"firstRow".into(), &first_row.into()).is_ok()
            && Reflect::set(&message, &"texels".into(), &Uint32Array::from(texels)).is_ok();
        if !ok {
            return false;
        }
        for worker in self.workers.iter().flatten() {
            let _ = worker.post_message(&message);
        }
        if let Some(fallback) = self.fallback.as_mut() {
            fallback.upload_atlas_rows(first_row, texels);
        }
        true
    }

    fn post_full_atlas(&self, band: usize) {
        let Some(Some(worker)) = self.workers.get(band) else {
            return;
        };
        let message = js_sys::Object::new();
        if Reflect::set(&message, &"type".into(), &"atlas".into()).is_ok()
            && Reflect::set(
                &message,
                &"texels".into(),
                &Uint32Array::from(self.atlas.as_slice()),
            )
            .is_ok()
        {
            let _ = worker.post_message(&message);
        }
    }

    /// Posts one frame to every idle, healthy band, then presents whatever
    /// complete frame has arrived. Never blocks on a worker.
    pub fn draw(
        &mut self,
        frame: &FrameParams,
        chunks: &[ChunkDraw],
        settings: crate::adapters::cpu_splatter::CpuRenderSettings,
    ) {
        self.resend_atlas_where_requested();

        let chunks_changed = chunks != self.last_chunks.as_slice();
        if chunks_changed {
            self.last_chunks = chunks.to_vec();
            self.chunks_version = self.chunks_version.wrapping_add(1);
        }

        let started = {
            let mut state = self.state.borrow_mut();
            // Every band renders the same frame or none does. Handing a new
            // frame only to the bands that happen to be free would leave
            // them on different frame numbers, and a frame is presented only
            // when they all agree — so they would never agree again.
            if !state.scheduler.ready_for_new_frame() {
                None
            } else {
                let frame_id = state.scheduler.begin_frame();
                let band_count = state.scheduler.band_count();
                let targets: Vec<usize> = (0..band_count)
                    .filter(|band| state.scheduler.should_post(*band))
                    .collect();
                for band in &targets {
                    state.scheduler.mark_posted(*band, frame_id);
                }
                Some((frame_id, targets))
            }
        };
        let Some((frame_id, targets)) = started else {
            // Still waiting on the pool. Present anything that completed
            // since the last tick and let this rAF go.
            self.draw_fallback_bands(frame, chunks);
            self.present_if_complete();
            return;
        };

        let band_count = self.band_count();
        for band in targets {
            // A worker that has never acked this table must receive it, even
            // when it did not change this frame.
            let request = RenderFrameRequest {
                frame_id,
                band_index: band as u32,
                band_count: band_count as u32,
                width: self.width,
                height: self.height,
                chunks_version: self.chunks_version,
                chunks: Some(self.last_chunks.clone()),
                frame: frame.clone(),
                settings,
            };
            if !self.post_frame(band, frame_id, &request) {
                self.state.borrow_mut().scheduler.mark_failed(band);
            }
        }

        self.draw_fallback_bands(frame, chunks);
        self.present_if_complete();
    }

    fn post_frame(&self, band: usize, frame_id: u32, request: &RenderFrameRequest) -> bool {
        let Some(Some(worker)) = self.workers.get(band) else {
            return false;
        };
        let encoded = encode_render_frame(request);
        let bytes = Uint8Array::from(encoded.as_slice());
        let message = js_sys::Object::new();
        let built = Reflect::set(&message, &"type".into(), &"frame".into()).is_ok()
            && Reflect::set(&message, &"frameId".into(), &frame_id.into()).is_ok()
            && Reflect::set(&message, &"width".into(), &self.width.into()).is_ok()
            && Reflect::set(&message, &"request".into(), &bytes).is_ok();
        built && worker.post_message(&message).is_ok()
    }

    fn resend_atlas_where_requested(&mut self) {
        let requested: Vec<usize> = {
            let mut state = self.state.borrow_mut();
            let bands: Vec<usize> = state
                .atlas_resend
                .iter()
                .enumerate()
                .filter_map(|(band, wanted)| wanted.then_some(band))
                .collect();
            for band in &bands {
                state.atlas_resend[*band] = false;
            }
            bands
        };
        for band in requested {
            self.post_full_atlas(band);
        }
    }

    /// Draws any band whose worker is gone, in-process, with the same
    /// band-assigned rasterizer that worker was running.
    fn draw_fallback_bands(&mut self, frame: &FrameParams, chunks: &[ChunkDraw]) {
        let orphaned: Vec<usize> = self.state.borrow().scheduler.fallback_bands().collect();
        if orphaned.is_empty() {
            return;
        }
        let band_count = self.band_count();
        let width = self.width.max(1) as usize;
        let height = self.height.max(1) as usize;
        let atlas = std::mem::take(&mut self.atlas);
        let rasterizer = self.fallback.get_or_insert_with(|| {
            let mut rasterizer = SoftwareRasterizer::new(width, height);
            rasterizer.upload_atlas(&atlas);
            rasterizer
        });
        rasterizer.settings = crate::get_cpu_settings();
        rasterizer.settings.toggles = crate::get_render_toggles();

        for band in orphaned {
            rasterizer.set_band_assignment(band_count, band);
            rasterizer.draw(frame, chunks);
            for (row_offset, rgba) in rasterizer.bands_rgba() {
                let rows = (rgba.len() / 4 / width) as u32;
                if let Ok(image) = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
                    wasm_bindgen::Clamped(rgba),
                    width as u32,
                    rows,
                ) {
                    let _ = self
                        .context
                        .put_image_data(&image, 0.0, f64::from(row_offset as u32));
                }
            }
        }
        self.atlas = atlas;
    }

    /// Composites the buffered bands once they all agree on a frame.
    ///
    /// Whole frames only: drawing each band as it lands would stack several
    /// different moments of the world on screen at once, visible as tearing
    /// between bands whenever the camera moves.
    fn present_if_complete(&mut self) {
        let ready = self.state.borrow_mut().scheduler.frame_ready_to_present();
        let Some(frame_id) = ready else {
            return;
        };
        let mut state = self.state.borrow_mut();
        for slot in state.pending.iter_mut() {
            let Some(band) = slot.as_ref() else { continue };
            if band.frame_id != frame_id {
                continue;
            }
            // `drawImage` takes an ImageBitmap source directly and is
            // GPU-composited. `transferFromImageBitmap` cannot be used here:
            // it replaces the entire canvas with one bitmap and has no
            // offset, so it cannot assemble a frame from several bands.
            let _ = self.context.draw_image_with_image_bitmap(
                &band.bitmap,
                0.0,
                f64::from(band.row_offset),
            );
            if let Some(band) = slot.take() {
                band.bitmap.close();
            }
        }
    }

    /// Per-band telemetry summed for the HUD: visited nodes, splats, pixel
    /// writes, deepest virtual level, and whether any band ran out of budget.
    pub fn telemetry_totals(&self) -> [u32; 5] {
        let state = self.state.borrow();
        let mut totals = [0u32; 5];
        for band in &state.telemetry {
            totals[0] = totals[0].saturating_add(band[0]);
            totals[1] = totals[1].saturating_add(band[1]);
            totals[2] = totals[2].saturating_add(band[2]);
            totals[3] = totals[3].max(band[3]);
            totals[4] |= band[4];
        }
        totals
    }
}

impl Drop for RenderWorkerPool {
    fn drop(&mut self) {
        for worker in self.workers.iter().flatten() {
            worker.terminate();
        }
    }
}

fn handle_worker_message(state: &Rc<RefCell<PoolState>>, band: usize, event: &MessageEvent) {
    let data = event.data();
    let Some(kind) = Reflect::get(&data, &"type".into())
        .ok()
        .and_then(|value| value.as_string())
    else {
        return;
    };

    match kind.as_str() {
        "rendered" => {
            let Some(frame_id) = exact_u32(&data, "frameId") else {
                return;
            };
            let row_offset = exact_u32(&data, "rowOffset").unwrap_or(0);
            let Ok(bitmap) = Reflect::get(&data, &"bitmap".into()) else {
                return;
            };
            let Ok(bitmap) = bitmap.dyn_into::<ImageBitmap>() else {
                return;
            };

            let mut state = state.borrow_mut();
            if !state.scheduler.mark_delivered(band, frame_id) {
                // Stale: the frame it belongs to has been abandoned. Release
                // the bitmap rather than leaking its backing store.
                bitmap.close();
                return;
            }
            if let Some(telemetry) = read_telemetry(&data) {
                state.telemetry[band] = telemetry;
            }
            if let Some(previous) = state.pending[band].replace(PendingBand {
                frame_id,
                row_offset,
                bitmap,
            }) {
                previous.bitmap.close();
            }
        }
        "skipped" => {
            // The worker could not draw this frame. Leave it in flight so it
            // is not counted complete; the next frame supersedes it.
            if let Some(frame_id) = exact_u32(&data, "frameId") {
                let mut state = state.borrow_mut();
                state.scheduler.mark_delivered(band, frame_id);
                state.pending[band] = None;
            }
        }
        "atlasRejected" => {
            state.borrow_mut().atlas_resend[band] = true;
        }
        "fatal" => {
            state.borrow_mut().scheduler.mark_failed(band);
        }
        _ => {}
    }
}

fn read_telemetry(data: &JsValue) -> Option<[u32; 5]> {
    let values = Reflect::get(data, &"telemetry".into()).ok()?;
    let values = values.dyn_into::<Uint32Array>().ok()?;
    if values.length() < 5 {
        return None;
    }
    let mut out = [0u32; 5];
    for (slot, value) in out.iter_mut().enumerate() {
        *value = values.get_index(slot as u32);
    }
    Some(out)
}

/// Reads a field that must be an exact non-negative integer, so a malformed
/// reply is dropped rather than silently truncated into a wrong band.
fn exact_u32(data: &JsValue, key: &str) -> Option<u32> {
    let value = Reflect::get(data, &key.into()).ok()?.as_f64()?;
    if !value.is_finite() || value < 0.0 || value > f64::from(u32::MAX) || value.fract() != 0.0 {
        return None;
    }
    Some(value as u32)
}
