#[cfg(target_arch = "wasm32")]
pub mod browser;
#[cfg(target_arch = "wasm32")]
pub mod console_telemetry;
#[cfg(target_arch = "wasm32")]
pub mod cpu_canvas;
#[cfg(target_arch = "wasm32")]
pub mod gl;
#[cfg(target_arch = "wasm32")]
pub mod shaders;
#[cfg(target_arch = "wasm32")]
pub mod splat_webgl;
#[cfg(target_arch = "wasm32")]
pub mod surface_webgl;
#[cfg(target_arch = "wasm32")]
pub mod surfel_webgl;
#[cfg(target_arch = "wasm32")]
pub mod webgl;
#[cfg(target_arch = "wasm32")]
pub mod worker_source;

// Pure request bookkeeping for the browser generation-worker driver. Keeping
// it free of web-sys makes failure and stale-message behavior natively
// testable even though Worker itself only exists on wasm32.
#[cfg(any(target_arch = "wasm32", test))]
mod generation_worker_requests;

// The byte layout `surfel_webgl` uploads through, kept free of web-sys for
// the same reason: an offset or byte order that is quietly wrong produces
// discs at plausible positions rather than an error, so it belongs where
// `cargo test` can reach it.
#[cfg(any(target_arch = "wasm32", test))]
pub mod surfel_layout;

pub mod cpu_reference_renderer;
pub mod file_archive;
pub mod fs_artifact_sink;
pub mod raymarch_reference_renderer;
pub mod webgpu;
