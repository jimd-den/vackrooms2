// Render worker: draws one horizontal band of the framebuffer off the main
// thread. Deliberately separate from worker.js — that one generates chunk
// geometry on demand and is addressed by chunk affinity; this one is a
// long-lived member of a small pool tied to the render loop. Merging the two
// dispatchers would couple two unrelated pool sizes and two unrelated
// lifecycles.
//
// The handler is registered synchronously at module scope so no message is
// lost while wasm instantiates, and work is serialized through a promise
// chain, exactly as worker.js does.
//
// Bands never share memory: without cross-origin isolation there is no
// SharedArrayBuffer, so this worker holds its own atlas copy, kept in step by
// the same incremental row uploads the main thread applies to its own.
//
// Protocol (main -> worker):
//   { type: "init", bandIndex, bandCount, width, height }
//   { type: "atlas", texels }                 Uint32Array, full replacement
//   { type: "atlasRows", firstRow, texels }   Uint32Array, incremental
//   { type: "frame", frameId, request }       request encoded by
//                                             adapters::render_frame_codec
// (worker -> main):
//   { type: "rendered", frameId, bandIndex, rowOffset, bitmap, telemetry }
//                                 bitmap transferred; one band's pixels
//   { type: "skipped", frameId, bandIndex }   nothing drawable this frame
//   { type: "fatal", message }                worker setup failed
import init, {
  render_worker_init,
  render_worker_upload_atlas,
  render_worker_upload_atlas_rows,
  render_worker_draw_band,
  render_worker_band_row_offset,
  render_worker_telemetry,
} from "./pkg/wasm_frontend.js";

let wasmReady = null;
function ensureWasm() {
  if (!wasmReady) wasmReady = init();
  return wasmReady;
}

let queue = Promise.resolve();
let bandIndex = 0;
// One canvas reused for the worker's lifetime. transferToImageBitmap hands
// off its contents and leaves the canvas cleared, so it never accumulates.
let canvas = null;
let context = null;
// Warn on the first skip only, so a persistent failure is visible without
// flooding the console at 60fps.
let skipWarned = false;

function errorMessage(error) {
  if (error instanceof Error) return error.message;
  return String(error);
}

function reportFatal(error) {
  const message = errorMessage(error);
  try {
    postMessage({ type: "fatal", bandIndex, message });
  } catch {
    // If even the failure cannot be posted, the pool notices via onerror or
    // by this band never delivering; say something locally regardless.
    console.error("render worker fatal:", message);
  }
}

function resizeSurface(width, height) {
  if (canvas && canvas.width === width && canvas.height === height) return;
  canvas = new OffscreenCanvas(width, height);
  // `alpha: false` lets the compositor skip per-pixel blending; the
  // rasterizer already writes every alpha byte as 255.
  context = canvas.getContext("2d", { alpha: false });
}

async function processMessage(message) {
  await ensureWasm();

  switch (message.type) {
    case "init":
      bandIndex = message.bandIndex >>> 0;
      render_worker_init(
        message.bandIndex >>> 0,
        message.bandCount >>> 0,
        message.width >>> 0,
        message.height >>> 0
      );
      return;

    case "atlas":
      render_worker_upload_atlas(message.texels);
      return;

    case "atlasRows":
      if (!render_worker_upload_atlas_rows(message.firstRow >>> 0, message.texels)) {
        // Validation rejected the block. Ask for a full resend rather than
        // drawing from an atlas that disagrees with the main thread's.
        postMessage({ type: "atlasRejected", bandIndex });
      }
      return;

    case "frame": {
      const rgba = render_worker_draw_band(message.request);
      const width = message.width >>> 0;
      const rows = width > 0 ? rgba.length / 4 / width : 0;
      if (rgba.length === 0 || rows < 1) {
        // Undecodable request, or a stale chunk table. Skipping costs this
        // band one frame; the compositor keeps showing the last whole one.
        //
        // Say so once. A band that can never draw produces a permanently
        // black screen with nothing in the console to explain it, which is
        // exactly how a malformed request went unnoticed before.
        if (!skipWarned) {
          skipWarned = true;
          console.warn(
            `render worker band ${bandIndex}: nothing drawable ` +
              `(request ${message.request?.length ?? 0} bytes, width ${width}). ` +
              `Further skips are silent.`
          );
        }
        postMessage({ type: "skipped", frameId: message.frameId, bandIndex });
        return;
      }
      skipWarned = false;

      resizeSurface(width, rows);
      context.putImageData(new ImageData(new Uint8ClampedArray(rgba.buffer), width, rows), 0, 0);
      const bitmap = canvas.transferToImageBitmap();
      postMessage(
        {
          type: "rendered",
          frameId: message.frameId,
          bandIndex,
          rowOffset: render_worker_band_row_offset(),
          telemetry: render_worker_telemetry(),
          bitmap,
        },
        [bitmap]
      );
      return;
    }

    default:
      throw new Error(`unknown render worker message: ${message.type}`);
  }
}

onmessage = (event) => {
  const message = event.data;
  // Each item catches independently: one bad frame must not leave the chain
  // rejected and silently swallow every frame after it.
  queue = queue.then(() => processMessage(message)).catch(reportFatal);
};
