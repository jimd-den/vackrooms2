//! Binary transport for one frame's render request, main thread to a render
//! worker.
//!
//! Separate from [`chunk_codec`](super::chunk_codec) on purpose. That format
//! carries `ChunkPayload` — whole geometry payloads produced by the
//! generation pipeline, tens to hundreds of kilobytes. This one carries the
//! per-frame draw call: a camera, a light list, and a chunk *draw table*.
//! They share a style (little-endian, length-prefixed, magic-versioned,
//! defensive decode) because it is the right style, not because they share a
//! concern.
//!
//! The chunk table is versioned rather than resent. It only changes when
//! chunks stream in or out, so at 60fps almost every frame can omit it and
//! let the worker reuse what it already holds.
//!
//! Platform-free and natively round-trip tested.

use crate::adapters::cpu_splatter::{CpuRenderSettings, CpuShadowMode};
use crate::application::ports::{
    ChunkDraw, DynamicLight, Environment, FrameParams, LightKind, LightSource,
};
use crate::application::render_settings::RenderToggles;

/// "VKR" + version 2. Version 2 carries the quality settings, which a
/// worker cannot read from its own instance's globals.
const MAGIC: u32 = 0x564B_5202;

/// Encoded size of the settings block, used by the tests to locate fields.
#[cfg(test)]
const SETTINGS_BYTES: usize = 7 * 4 + 4 + 2;

// Exact encoded sizes, used to bound length prefixes against the bytes that
// actually remain. These must be the *true* sizes: a value larger than the
// real encoding rejects valid buffers once the count is high enough, which
// is a bug that only appears at realistic scene sizes.
/// origin(12) + root(4) + world_size(4) + voxel_size(4) + svo_depth(1)
const CHUNK_BYTES: usize = 25;
/// id(8) + position(12) + half_size(8) + colour(12) + radius(4)
/// + intensity(4) + kind(1) + flicker(1) + enabled(1)
const LIGHT_BYTES: usize = 51;
/// position(12) + colour(12) + radius(4) + intensity(4)
const DYNAMIC_LIGHT_BYTES: usize = 32;

/// A frame as it crosses the worker boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderFrameRequest {
    /// Monotonic frame id, echoed back so the compositor can tell which
    /// bands belong together.
    pub frame_id: u32,
    /// Which band of the framebuffer the worker owns.
    pub band_index: u32,
    pub band_count: u32,
    pub width: u32,
    pub height: u32,
    /// Bumped whenever `chunks` actually changes. When `chunks` is `None`
    /// the worker must reuse the table it cached under this same version.
    pub chunks_version: u32,
    pub chunks: Option<Vec<ChunkDraw>>,
    pub frame: FrameParams,
    /// Quality settings, sent explicitly because a worker cannot read them.
    /// The settings atomics live in each wasm instance's own memory, so a
    /// worker's copy is whatever `Default` left there — it would silently
    /// render at a different FOV and detail level from the main thread.
    pub settings: CpuRenderSettings,
}

pub fn encode_render_frame(request: &RenderFrameRequest) -> Vec<u8> {
    let frame = &request.frame;
    let mut out = Vec::with_capacity(
        128 + request.chunks.as_ref().map_or(0, |chunks| chunks.len() * 32)
            + frame.scene_lights.len() * 56
            + frame.active_dynamic_lights().len() * 32,
    );
    put_u32(&mut out, MAGIC);
    put_u32(&mut out, request.frame_id);
    put_u32(&mut out, request.band_index);
    put_u32(&mut out, request.band_count);
    put_u32(&mut out, request.width);
    put_u32(&mut out, request.height);
    put_u32(&mut out, request.chunks_version);
    put_settings(&mut out, &request.settings);

    match &request.chunks {
        None => out.push(0),
        Some(chunks) => {
            out.push(1);
            put_u32(&mut out, chunks.len() as u32);
            for chunk in chunks {
                for c in chunk.origin {
                    put_f32(&mut out, c);
                }
                put_u32(&mut out, chunk.root_index as u32);
                put_f32(&mut out, chunk.world_size);
                put_f32(&mut out, chunk.voxel_size);
                out.push(chunk.svo_depth);
            }
        }
    }

    for c in frame.camera_pos {
        put_f32(&mut out, c);
    }
    put_f32(&mut out, frame.yaw);
    put_f32(&mut out, frame.pitch);
    out.push(u8::from(frame.flashlight));
    put_environment(&mut out, &frame.environment);

    put_u32(&mut out, frame.scene_lights.len() as u32);
    for light in &frame.scene_lights {
        put_light(&mut out, light);
    }

    let dynamic = frame.active_dynamic_lights();
    put_u32(&mut out, dynamic.len() as u32);
    for light in dynamic {
        for c in light.position {
            put_f32(&mut out, c);
        }
        for c in light.color {
            put_f32(&mut out, c);
        }
        put_f32(&mut out, light.radius);
        put_f32(&mut out, light.intensity);
    }

    out
}

/// Decodes a request, or `None` for anything truncated or malformed.
///
/// The caller drops a `None` and the next frame supersedes it, so a bad
/// buffer costs one frame in one band rather than a panicked worker.
pub fn decode_render_frame(bytes: &[u8]) -> Option<RenderFrameRequest> {
    let mut r = Reader { bytes, pos: 0 };
    if r.u32()? != MAGIC {
        return None;
    }
    let frame_id = r.u32()?;
    let band_index = r.u32()?;
    let band_count = r.u32()?;
    let width = r.u32()?;
    let height = r.u32()?;
    let chunks_version = r.u32()?;
    let settings = r.settings()?;

    let chunks = match r.u8()? {
        0 => None,
        1 => {
            let count = r.len(CHUNK_BYTES)?;
            let mut chunks = Vec::with_capacity(count);
            for _ in 0..count {
                chunks.push(ChunkDraw {
                    origin: [r.f32()?, r.f32()?, r.f32()?],
                    root_index: r.u32()? as i32,
                    world_size: r.f32()?,
                    voxel_size: r.f32()?,
                    svo_depth: r.u8()?,
                });
            }
            Some(chunks)
        }
        _ => return None,
    };

    let camera_pos = [r.f32()?, r.f32()?, r.f32()?];
    let yaw = r.f32()?;
    let pitch = r.f32()?;
    let flashlight = r.u8()? != 0;
    let environment = r.environment()?;

    let scene_light_count = r.len(LIGHT_BYTES)?;
    let mut scene_lights = Vec::with_capacity(scene_light_count);
    for _ in 0..scene_light_count {
        scene_lights.push(r.light()?);
    }

    let dynamic_count = r.len(DYNAMIC_LIGHT_BYTES)?;
    let mut frame = FrameParams {
        camera_pos,
        yaw,
        pitch,
        flashlight,
        scene_lights,
        environment,
        ..FrameParams::default()
    };
    if dynamic_count > frame.dynamic_lights.len() {
        return None;
    }
    for slot in 0..dynamic_count {
        frame.dynamic_lights[slot] = DynamicLight {
            position: [r.f32()?, r.f32()?, r.f32()?],
            color: [r.f32()?, r.f32()?, r.f32()?],
            radius: r.f32()?,
            intensity: r.f32()?,
        };
    }
    frame.dynamic_light_count = dynamic_count as u8;

    Some(RenderFrameRequest {
        frame_id,
        band_index,
        band_count,
        width,
        height,
        chunks_version,
        chunks,
        frame,
        settings,
    })
}

fn put_settings(out: &mut Vec<u8>, settings: &CpuRenderSettings) {
    put_f32(out, settings.internal_scale);
    put_f32(out, settings.lod_cutoff_px);
    put_f32(out, settings.max_splat_radius_px);
    put_f32(out, settings.min_split_size);
    put_f32(out, settings.max_draw_distance);
    put_f32(out, settings.min_mip_occupancy);
    put_f32(out, settings.fov_tan);
    put_u32(out, settings.toggles.to_bits());
    out.push(settings.max_virtual_depth);
    out.push(settings.shadows.id() as u8);
}

fn put_environment(out: &mut Vec<u8>, environment: &Environment) {
    for c in environment.sky_color {
        put_f32(out, c);
    }
    for c in environment.fog_color {
        put_f32(out, c);
    }
    put_f32(out, environment.fog_density);
    put_f32(out, environment.fog_start);
    put_f32(out, environment.ambient_scale);
    out.push(u8::from(environment.outdoor));
}

fn put_light(out: &mut Vec<u8>, light: &LightSource) {
    out.extend_from_slice(&light.id.to_le_bytes());
    for c in light.position {
        put_f32(out, c);
    }
    for c in light.half_size {
        put_f32(out, c);
    }
    for c in light.color {
        put_f32(out, c);
    }
    put_f32(out, light.radius);
    put_f32(out, light.intensity);
    out.push(light.kind as u8);
    out.push(light.flicker_mode);
    out.push(u8::from(light.enabled));
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn slice(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.slice(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.slice(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.slice(8)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.slice(4)?.try_into().ok()?))
    }
    fn settings(&mut self) -> Option<CpuRenderSettings> {
        Some(CpuRenderSettings {
            internal_scale: self.f32()?,
            lod_cutoff_px: self.f32()?,
            max_splat_radius_px: self.f32()?,
            min_split_size: self.f32()?,
            max_draw_distance: self.f32()?,
            min_mip_occupancy: self.f32()?,
            fov_tan: self.f32()?,
            toggles: RenderToggles::from_bits(self.u32()?),
            max_virtual_depth: self.u8()?,
            shadows: CpuShadowMode::from_id(u32::from(self.u8()?)),
        })
    }

    fn environment(&mut self) -> Option<Environment> {
        Some(Environment {
            sky_color: [self.f32()?, self.f32()?, self.f32()?],
            fog_color: [self.f32()?, self.f32()?, self.f32()?],
            fog_density: self.f32()?,
            fog_start: self.f32()?,
            ambient_scale: self.f32()?,
            outdoor: self.u8()? != 0,
        })
    }
    fn light(&mut self) -> Option<LightSource> {
        Some(LightSource {
            id: self.u64()?,
            position: [self.f32()?, self.f32()?, self.f32()?],
            half_size: [self.f32()?, self.f32()?],
            color: [self.f32()?, self.f32()?, self.f32()?],
            radius: self.f32()?,
            intensity: self.f32()?,
            kind: LightKind::from_u8(self.u8()?)?,
            flicker_mode: self.u8()?,
            enabled: self.u8()? != 0,
        })
    }
    /// Reads a length prefix and bounds it against the bytes that actually
    /// remain, so a corrupt count cannot force a huge allocation.
    fn len(&mut self, min_element_bytes: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        let remaining = self.bytes.len().saturating_sub(self.pos);
        if n.saturating_mul(min_element_bytes.max(1)) > remaining {
            return None;
        }
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_light(id: u64, kind: LightKind) -> LightSource {
        LightSource {
            id,
            position: [1.5, 2.5, -3.5],
            half_size: [0.5, 0.25],
            color: [1.0, 0.9, 0.8],
            radius: 12.0,
            intensity: 3.5,
            kind,
            flicker_mode: 2,
            enabled: true,
        }
    }

    fn sample_request(chunks: Option<Vec<ChunkDraw>>) -> RenderFrameRequest {
        let mut frame = FrameParams {
            camera_pos: [4.0, 1.75, -6.25],
            yaw: 1.25,
            pitch: -0.5,
            flashlight: true,
            scene_lights: vec![
                sample_light(7, LightKind::CeilingPanel),
                sample_light(9, LightKind::Strip),
                sample_light(11, LightKind::Emergency),
            ],
            environment: Environment {
                sky_color: [0.1, 0.2, 0.3],
                fog_color: [0.05, 0.05, 0.06],
                fog_density: 0.02,
                fog_start: 9.5,
                ambient_scale: 0.75,
                outdoor: false,
            },
            ..FrameParams::default()
        };
        frame.dynamic_lights[0] = DynamicLight {
            position: [0.5, 1.0, 2.0],
            color: [1.0, 0.4, 0.1],
            radius: 6.0,
            intensity: 2.25,
        };
        frame.dynamic_light_count = 1;

        RenderFrameRequest {
            frame_id: 4242,
            band_index: 2,
            band_count: 4,
            width: 960,
            height: 540,
            chunks_version: 17,
            chunks,
            frame,
            settings: CpuRenderSettings {
                fov_tan: 0.9,
                max_draw_distance: 77.0,
                ..CpuRenderSettings::default()
            },
        }
    }

    fn sample_chunks() -> Vec<ChunkDraw> {
        vec![
            ChunkDraw {
                origin: [0.0, 0.0, 0.0],
                root_index: 3,
                world_size: 32.0,
                voxel_size: 0.5,
                svo_depth: 6,
            },
            ChunkDraw {
                origin: [-32.0, 0.0, 64.0],
                root_index: 4096,
                world_size: 32.0,
                voxel_size: 1.0,
                svo_depth: 5,
            },
        ]
    }

    #[test]
    fn round_trips_a_frame_carrying_the_chunk_table() {
        let request = sample_request(Some(sample_chunks()));
        let decoded = decode_render_frame(&encode_render_frame(&request)).expect("decodes");
        assert_eq!(decoded, request);
    }

    #[test]
    fn round_trips_a_frame_that_reuses_the_cached_chunk_table() {
        let request = sample_request(None);
        let decoded = decode_render_frame(&encode_render_frame(&request)).expect("decodes");
        assert_eq!(decoded, request);
        assert!(decoded.chunks.is_none());
        assert_eq!(decoded.chunks_version, 17);
    }

    #[test]
    fn omitting_the_chunk_table_is_much_smaller() {
        // The whole point of versioning the table: a steady-state frame
        // should not pay for a chunk list that has not changed.
        let with = encode_render_frame(&sample_request(Some(sample_chunks())));
        let without = encode_render_frame(&sample_request(None));
        assert!(
            without.len() + 32 < with.len(),
            "{} vs {}",
            without.len(),
            with.len()
        );
    }

    #[test]
    fn every_truncation_is_rejected_rather_than_panicking() {
        let encoded = encode_render_frame(&sample_request(Some(sample_chunks())));
        for cut in 0..encoded.len() {
            assert!(
                decode_render_frame(&encoded[..cut]).is_none(),
                "a {cut}-byte prefix must not decode"
            );
        }
    }

    #[test]
    fn a_foreign_or_corrupt_buffer_is_rejected() {
        assert!(decode_render_frame(&[]).is_none());
        assert!(decode_render_frame(&[0xFF; 256]).is_none());

        let mut wrong_magic = encode_render_frame(&sample_request(None));
        wrong_magic[0] ^= 0xFF;
        assert!(decode_render_frame(&wrong_magic).is_none());
    }

    #[test]
    fn an_absurd_chunk_count_cannot_force_an_allocation() {
        let mut encoded = encode_render_frame(&sample_request(Some(sample_chunks())));
        // The count sits after the header words, the settings block, and
        // the table-present flag.
        let count_at = 4 * 7 + SETTINGS_BYTES + 1;
        encoded[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_render_frame(&encoded).is_none());
    }

    /// A real frame carries far more than the handful the round-trip tests
    /// use, and the length bound is per-element — so it only bites at scale.
    #[test]
    fn a_realistic_light_and_chunk_count_still_decodes() {
        for (lights, chunk_count) in [(1usize, 1usize), (8, 4), (64, 25), (256, 64)] {
            let mut request = sample_request(Some(
                (0..chunk_count)
                    .map(|i| ChunkDraw {
                        origin: [i as f32 * 8.0, 0.0, 0.0],
                        root_index: i as i32,
                        world_size: 32.0,
                        voxel_size: 0.5,
                        svo_depth: 6,
                    })
                    .collect(),
            ));
            request.frame.scene_lights = (0..lights)
                .map(|i| sample_light(i as u64, LightKind::CeilingPanel))
                .collect();

            let encoded = encode_render_frame(&request);
            assert_eq!(
                decode_render_frame(&encoded).as_ref(),
                Some(&request),
                "{lights} lights and {chunk_count} chunks failed to decode \
                 ({} bytes)",
                encoded.len()
            );
        }
    }

    #[test]
    fn the_length_bounds_match_what_the_encoder_actually_writes() {
        // A bound larger than the true element size rejects valid buffers
        // at scale; smaller than it weakens the allocation guard. Measure
        // both against the encoder rather than trusting arithmetic.
        let one = encode_render_frame(&sample_request(Some(sample_chunks()[..1].to_vec())));
        let two = encode_render_frame(&sample_request(Some(sample_chunks())));
        assert_eq!(two.len() - one.len(), CHUNK_BYTES, "chunk");

        let mut with_one_light = sample_request(None);
        with_one_light.frame.scene_lights = vec![sample_light(1, LightKind::Point)];
        let mut with_two_lights = sample_request(None);
        with_two_lights.frame.scene_lights = vec![
            sample_light(1, LightKind::Point),
            sample_light(2, LightKind::Strip),
        ];
        assert_eq!(
            encode_render_frame(&with_two_lights).len()
                - encode_render_frame(&with_one_light).len(),
            LIGHT_BYTES,
            "light"
        );

        let mut with_two_dynamic = sample_request(None);
        with_two_dynamic.frame.dynamic_light_count = 2;
        assert_eq!(
            encode_render_frame(&with_two_dynamic).len()
                - encode_render_frame(&sample_request(None)).len(),
            DYNAMIC_LIGHT_BYTES,
            "dynamic light"
        );
    }

    #[test]
    fn an_unknown_light_kind_is_rejected() {
        // `LightKind` owns the tag mapping (`ports.rs`); this only checks the
        // codec routes through it rather than keeping a second copy that
        // could drift when a variant is added.
        assert!(LightKind::from_u8(4).is_none());
        for kind in [
            LightKind::Point,
            LightKind::CeilingPanel,
            LightKind::Strip,
            LightKind::Emergency,
        ] {
            assert_eq!(LightKind::from_u8(kind as u8), Some(kind));
        }
    }
}
