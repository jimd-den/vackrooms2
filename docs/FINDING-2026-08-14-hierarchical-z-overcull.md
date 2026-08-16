# Finding: hierarchical-Z rejects geometry the fine depth test would draw

- **Date:** 2026-08-14
- **Status:** open, pre-existing — not introduced by the band-partitioning work
- **Found by:** the band-equivalence tests added in `cpu_splatter/tests.rs`

## The measurement

One fixture (`dense_block_atlas`, two 8³ hollow shells at 96×64, camera
`[4, 4, -6]` looking `+z`), rendered unpartitioned, counting successful fine
depth writes:

| `hierarchical_z` | pixel writes |
|---|---|
| `false` (ground truth: every node traversed, exact per-pixel depth test) | **4389** |
| `true` (default) | **4383** |

Hierarchical-Z is supposed to be a pure accelerator: it may only skip work the
fine depth test would have rejected anyway. It is instead **losing six pixel
writes** — geometry that the exact depth test does accept.

Reproduce by rendering the fixture both ways and comparing
`telemetry().pixel_writes`; the two framebuffers also differ.

## Why it is probably happening

`project_voxel_node` (`rasterizer/traverse_voxel_scene/project_voxel_node.rs:51`)
rejects a node when `coarse_rect_occludes` reports every covered tile fully
covered and nearer than `footprint.nearest_depth`. The query uses the
projected **AABB** rectangle, but the splat that node eventually writes is a
**square kernel** centred on the projection with its own half-extent
(`SquareSplat`, `write_depth_tested_splats.rs`). Where the kernel reaches
outside the AABB rectangle the query was never asked about those pixels, so a
node can be culled while some of its kernel would have passed the depth test.

That is a hypothesis consistent with the numbers, not a confirmed diagnosis.
The next step is to instrument the cull site: for each node HZ rejects,
re-run the fine test it would have performed and assert zero passing pixels.
That turns the hypothesis into either a fix or a counter-example.

## Why it matters beyond six pixels

The same gap is what stops the band-partitioned renderer from being
bit-identical to the unpartitioned one with HZ on. A band stores only its own
rows, so for a footprint crossing a band edge it cannot answer the occlusion
query and declines to cull — drawing *more* than the whole-frame target, which
by the argument above is the more correct answer. See the note above
`band_count_does_not_change_the_composited_frame` in
`wasm_frontend/src/adapters/cpu_splatter/tests.rs`; those tests therefore pin
exactness with HZ off, and `hierarchical_z_culls_more_when_unpartitioned`
pins the direction of the difference so it cannot widen unnoticed.

Fixing the over-cull would shrink, though not necessarily eliminate, that
divergence: fewer wrongly-culled nodes means fewer nodes whose culling can
differ between configurations.
