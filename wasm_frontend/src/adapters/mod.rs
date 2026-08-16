//! Interface adapters: translate between the application layer's ports and
//! the concrete outside world (browser input events, the procedural
//! generation core).

pub mod archived_chunk_source;
pub mod block_cache;
pub mod chunk_codec;
pub mod collect_emissive_lights;
pub mod cpu_splatter;
pub mod cpu_surfel;
pub mod face_instances;
pub mod input;
pub mod local_chunk_source;
pub mod query_config;
pub mod render_band_scheduler;
pub mod render_frame_codec;
pub mod section_locator;
pub mod surface_mesh;
pub mod surfel_cloud;
