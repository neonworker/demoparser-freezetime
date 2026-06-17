# Decoding CS2 smoke-grenade voxels (`m_VoxelFrameData`)

This document describes the on-wire format of the CS2 smoke-grenade voxel
journal and how this parser decodes it into usable 3-D geometry. It also records
how each part of the format was validated, and — importantly — what the data
does **not** contain.

The decoder lives in [`src/parser/src/second_pass/voxel.rs`](../src/parser/src/second_pass/voxel.rs).

## Prior art / credit

The **seed-frame** layout was first decoded by the
[`osztenkurden/cs2parser`](https://github.com/osztenkurden/cs2parser) project,
which extracts the coarse seed point set. That work is the starting point for
this module. As of this writing that project decodes only the seed; it does not
interpret the per-tick *extended* frames. This module reproduces the seed decode
and adds:

* the full **extended-frame occupancy** (a 32³ Morton-ordered bitset),
* **per-tick** reconstruction of that occupancy, and
* **world-space anchoring** of the voxels.

Everything below was derived independently from analysis of demo bytes; no
third-party code is incorporated.

## Where the data comes from

Each smoke grenade networks a `C_SmokeGrenadeProjectile` entity. Its
`m_VoxelFrameData` field (`C_NetworkUtlVectorBase<uint8>`) is an append-only
journal of fixed-grammar frames, and `m_nVoxelFrameDataSize` is the authoritative
byte length. This parser reassembles the journal into
[`SmokeRecord::voxel_frame_data`](../src/parser/src/second_pass/collect_data.rs)
at entity-delete time; `voxel.rs` then decodes it.

## Frame grammar

The journal is a sequence of length-prefixed frames:

```
[u16 seq][u16 len][payload (len bytes)]
```

`payload[1]` is a `sectionFlags` byte (`sf`). Frames fall into four buckets:

| Bucket      | Condition                       | Carries                                   |
|-------------|---------------------------------|-------------------------------------------|
| `Heartbeat` | `len == 3` and all-zero         | nothing (keep-alive)                      |
| `Seed`      | `sf & 1`                        | seed footprint **and** an extended section |
| `Extended`  | `sf & 2`                        | occupancy word updates                    |
| `Other`     | none of the above               | nothing — observed only as constant `01 00 00` keep-alive |

Only `Extended` (and the extended section inside the one `Seed` frame) carries
occupancy data. There is exactly one `Seed` frame per smoke, at the start.

## Extended frames — the 32³ occupancy bitset

This is the core of the smoke geometry. The smoke volume is a **32×32×32
occupancy bitset** = 32768 voxels = **512 words × 64 bits**.

The extended section is:

```
[u16 count][ count × (u16 word, u64 state) ][u8 trailing = 0]
```

* `word` is a 9-bit index `0..=511` into the 512-word bitset (the upper bits of
  the u16 are always zero — there is no opcode there).
* `state` is that word's 64-bit value, little-endian.
* Each **set bit** `b` (`0..63`) of `state` is one occupied voxel whose 15-bit
  **Morton code** is `(word << 6) | b`.

A Morton code is de-interleaved into `(x, y, z)`, each `0..31`, by keeping every
third bit (`compact1` / `morton_decode3` in the module).

### Per-tick reconstruction

Fold the extended frames **in sequence order** into a live `word -> u64` map:
each frame **overwrites** the words it carries. Bits may turn **on or off** — a
word re-sent with fewer bits clears those voxels (see "what the OFF bits mean"
below). The occupied voxel set at any point is every set bit across all live
words.

To anchor a reconstruction to game ticks, sample `m_nVoxelFrameDataSize` over
ticks to get a `tick -> cumulative-bytes` series, then a frame becomes "live" at
the first tick whose cumulative size ≥ that frame's `end_offset` (exposed on
`Frame`). The byte-size series is an application-level concern and is not part of
`SmokeRecord`.

### World anchoring

A voxel maps to world space by:

```
world = detonate_pos + (voxel - 16) * 20      (per axis)
```

i.e. voxel index 16 is the grid centre and sits on the detonation position, and
each voxel edge is 20 Source units (`voxel_to_world`). **Caveat:** some 2-D
top-down (radar) overlays need the **X** axis negated to match the target
image's handedness; `voxel_to_world` uses the engine convention directly and
callers can flip X downstream if needed.

## Seed frames — the coarse footprint

The seed frame's seed section is:

```
[u8 count][ count × (u8 x, u8 y, u8 z, u8 flag, u8×4 padding) ]
```

The points are in the **same 0..31 grid frame** as the occupancy. `x`/`y` are
the horizontal footprint plane and `z` the (near-constant) height. `flag` is `5`
for the common perimeter points and `0` for the rarer central ones; the four
padding bytes are always zero.

The seed is a **coarse ~2-voxel-spaced lattice sampling of the smoke's 2-D ground
footprint** (~44 points, ~13% of the full footprint columns). It is emitted in
the first frame, before the full occupancy builds up. Because it is a flat
footprint rather than a 3-D set, it is largely **redundant** with the occupancy
(its XY projection is a subset of the occupancy's) — it is decoded here for
completeness and for the instant-footprint use case.

## What the data does *not* contain

The journal carries **occupancy only** — which voxels the smoke volume fills. It
does **not** carry:

* **per-voxel density / opacity** (how see-through each voxel is), and
* **transient disturbances** — the temporary holes an HE detonation or a bullet
  opens, which then close again.

These are computed **client-side** at render time and are never serialised into
the demo. This was established by byte-accounting the entire journal (every byte
is occupancy, seed, or a keep-alive — there is no density channel) and by
confirming that nothing in the journal, in any other networked entity property,
or in the particle-manager message stream reacts to HE/bullet events near a live
smoke. A voxel that is "shot through" keeps its occupancy bit set; only its
client-side opacity drops. Reproducing those effects therefore requires
simulating the client volumetrics, not decoding the demo.

## Validation

Each part of the decoder was checked against real demos (multiple maps,
thousands of smokes):

* **Occupancy is coherent.** The seed frame's extended section decodes to ~644
  voxels forming a smoke-sized blob — centroid ≈ (16, 16, 16) (the grid centre,
  slightly high in Z as smoke rises) and mean radius ≈ 6.8 voxels ≈ 137 units,
  matching the ~144-unit in-game smoke radius. All voxels in range.
* **The format is byte-complete.** Walking the grammar consumes 100.000% of
  `m_nVoxelFrameDataSize` with no leftover bytes; every `word` is ≤ 511; the
  extended framing check (`len - off == 2 + count*10 + 1`) holds for every
  extended frame; the trailing byte is always 0; there is exactly one seed frame
  per smoke. There is no unparsed region and no hidden opcode.
* **Occupancy is collision-aware.** The (rare) voxels that turn *off* mid-life
  form persistent voids that are world-anchored — the same world location clears
  across repeated throws of the same lineup, and the voids are shaped like the
  solid features (pillars/railings) the smoke wraps around. This both confirms
  the overwrite/clear semantics and shows the volume conforms to map geometry.
* **The seed is the footprint.** Projecting the 44 seed points onto the
  horizontal plane gives 100% overlap with the occupancy's footprint, with
  `byte0/byte1/byte2 -> x/y/z` resolved unambiguously (100% vs 93.5% for the
  swapped axes).

## API summary

```rust
use parser::second_pass::voxel;

// From a parsed SmokeRecord:
let decoded = smoke.decode_voxels();          // seed_footprint + final_occupancy
let world   = smoke.occupancy_world();         // Vec<[f32;3]>

// Or directly from bytes, with per-frame control:
for frame in voxel::split_frames(&blob, size) { /* frame.kind, frame.end_offset */ }
let mut occ = voxel::Occupancy::new();
occ.apply(&frame);                              // fold in sequence order
let voxels = occ.voxels();                      // Vec<(u8,u8,u8)>, each 0..31
let w = voxel::voxel_to_world((x, y, z), detonate_pos);

// Compact per-frame deltas (for persisting a decoded volume; see below):
let deltas = voxel::delta_frames(&blob, size); // Vec<VoxelDelta>{ end_offset, added, removed }
```

## Compact delta form for storage

When persisting the *decoded* volume (rather than re-decoding the raw blob
downstream), use [`delta_frames`]. It emits, per extended frame, only the voxels
that turned **on**/**off** vs the previous frame, as 15-bit **column codes**
`(x << 10) | (y << 5) | z` (via `pack_voxel`/`unpack_voxel`) — not Morton, so a
consumer recovers `(x, y, z)` with three shifts and no de-interleave logic.

Two properties make this both small and faithful:

* **Delta exploits additivity.** Occupancy grows monotonically (apart from the
  rare collision voids in `removed`), so per-frame deltas are small; accumulating
  `added` minus `removed` in order losslessly reproduces every frame.
* **Native cadence front-loads fidelity.** The parser emits frames densely during
  the early bloom and sparsely once the volume settles (~⅔ of frames fall in the
  first quarter of a smoke's life), so keeping *every* frame captures the fluid
  early spread at full resolution while staying compact.

Anchor each delta to the game-tick timeline with the per-tick
`m_nVoxelFrameDataSize` series: a delta is live at the first tick whose cumulative
size ≥ its `end_offset`.
