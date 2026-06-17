//! Smoke-grenade voxel-journal decoding
//! (`C_SmokeGrenadeProjectile.m_VoxelFrameData`).
//!
//! The raw journal is captured per smoke as [`SmokeRecord::voxel_frame_data`].
//! This module turns that opaque byte blob into structured smoke geometry:
//!
//!   * the **extended-frame occupancy** — a 32×32×32 Morton-ordered bitset of
//!     which voxels the smoke volume fills, reconstructable per tick, and
//!   * the **seed footprint** — a coarse 2-D lattice sampling of the smoke's
//!     ground footprint emitted in the first frame.
//!
//! The on-wire format and the validation behind this decoder are documented in
//! `docs/smoke-voxel-decoding.md`.
//!
//! Prior art: the seed-frame layout was first decoded by the
//! [`osztenkurden/cs2parser`](https://github.com/osztenkurden/cs2parser)
//! project. This module reproduces that and adds the full extended-frame
//! occupancy bitset and its world-space anchoring.

use crate::second_pass::collect_data::SmokeRecord;
use std::collections::{BTreeMap, BTreeSet};

/// Occupancy grid resolution per axis (32³ = 32768 voxels = 512 words × 64 bits).
pub const GRID: u8 = 32;
/// Grid centre index. Voxel index 16 maps to the detonation position in world space.
pub const GRID_CENTRE: f32 = 16.0;
/// World size of one voxel edge, in Source units.
pub const VOXEL_SIZE: f32 = 20.0;

/// The four journal frame buckets. Only [`FrameKind::Extended`] (and the
/// extended section of a [`FrameKind::Seed`] frame) carries occupancy data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `len == 3` all-zero keep-alive.
    Heartbeat,
    /// `sectionFlags & 1`: carries the seed footprint (plus an extended section).
    Seed,
    /// `sectionFlags & 2`: carries occupancy word updates.
    Extended,
    /// Anything else — observed only as the constant `01 00 00` keep-alive.
    Other,
}

/// A single length-prefixed journal frame: `[u16 seq][u16 len][payload]`.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub seq: u16,
    pub kind: FrameKind,
    pub payload: &'a [u8],
    /// Byte offset of the end of this frame within the journal. Use this to
    /// anchor a per-tick reconstruction against `m_nVoxelFrameDataSize` sampled
    /// over ticks (see the docs).
    pub end_offset: usize,
}

/// Split the journal blob into its frames. `size` is the authoritative
/// `m_nVoxelFrameDataSize`; bytes past it (or a truncated trailing frame) are
/// ignored.
pub fn split_frames(blob: &[u8], size: usize) -> Vec<Frame<'_>> {
    let end = size.min(blob.len());
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= end {
        let seq = u16::from_le_bytes([blob[off], blob[off + 1]]);
        let len = u16::from_le_bytes([blob[off + 2], blob[off + 3]]) as usize;
        let po = off + 4;
        if po + len > end {
            break;
        }
        let payload = &blob[po..po + len];
        let sf = if payload.len() >= 2 { payload[1] } else { 0 };
        let heartbeat = len == 3 && payload.iter().all(|&b| b == 0);
        let kind = if heartbeat {
            FrameKind::Heartbeat
        } else if sf & 1 != 0 {
            FrameKind::Seed
        } else if sf & 2 != 0 {
            FrameKind::Extended
        } else {
            FrameKind::Other
        };
        out.push(Frame { seq, kind, payload, end_offset: po + len });
        off = po + len;
    }
    out
}

/// One point of the coarse seed footprint. `x`, `y`, `z` are in the same 0..31
/// grid frame as the occupancy voxels — `x`,`y` are the (horizontal) footprint
/// plane and `z` the near-constant height. `flag` is `5` for the common
/// (perimeter) points and `0` for the rarer central ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedPoint {
    pub x: u8,
    pub y: u8,
    pub z: u8,
    pub flag: u8,
}

/// Decode the seed footprint from a [`FrameKind::Seed`] frame's payload.
/// Layout: `payload[2]` = point count, then `count` × 8-byte records
/// `[x, y, z, flag, 0, 0, 0, 0]`.
pub fn decode_seed(payload: &[u8]) -> Vec<SeedPoint> {
    if payload.len() < 3 {
        return Vec::new();
    }
    let count = payload[2] as usize;
    let mut out = Vec::with_capacity(count);
    let mut o = 3usize;
    for _ in 0..count {
        if o + 8 > payload.len() {
            break;
        }
        out.push(SeedPoint { x: payload[o], y: payload[o + 1], z: payload[o + 2], flag: payload[o + 3] });
        o += 8;
    }
    out
}

/// De-interleave one axis of a 15-bit Morton code into a 0..31 coordinate.
#[inline]
fn compact1(mut v: u32) -> u32 {
    v &= 0x0924_9249;
    v = (v ^ (v >> 2)) & 0x030c_30c3;
    v = (v ^ (v >> 4)) & 0x0300_f00f;
    v = (v ^ (v >> 8)) & 0x0000_0fff;
    v
}

/// Decode a 15-bit Morton code into `(x, y, z)`, each 0..31.
#[inline]
pub fn morton_decode3(m: u32) -> (u8, u8, u8) {
    (compact1(m) as u8, compact1(m >> 1) as u8, compact1(m >> 2) as u8)
}

/// One extended-frame occupancy record: a 64-bit word of the 32³ bitset.
/// `word` (0..=511) is the high 9 bits of each voxel's Morton code; each set bit
/// `b` of `bits` is an occupied voxel with Morton code `(word << 6) | b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccupancyRecord {
    pub word: u16,
    pub bits: u64,
}

/// Decode the occupancy records carried by one extended frame (i.e. any frame
/// whose `sectionFlags & 2` is set, including seed frames). Returns an empty
/// vec if the frame carries no extended section or fails the framing check.
pub fn decode_extended(frame: &Frame) -> Vec<OccupancyRecord> {
    let p = frame.payload;
    let sf = if p.len() >= 2 { p[1] } else { 0 };
    if sf & 2 == 0 {
        return Vec::new();
    }
    // Extended section begins after the (optional) seed section.
    let mut o = 2usize;
    if sf & 1 != 0 {
        if p.len() < 3 {
            return Vec::new();
        }
        let c_occ = p[2] as usize;
        o = 3 + c_occ * 8;
    }
    if o + 2 > p.len() {
        return Vec::new();
    }
    let count = u16::from_le_bytes([p[o], p[o + 1]]) as usize;
    // Framing: [u16 count][count × (u16 word + u64 state)][1 trailing byte].
    if p.len() - o != 2 + count * 10 + 1 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let ro = o + 2 + i * 10;
        let word = u16::from_le_bytes([p[ro], p[ro + 1]]);
        let mut bits = 0u64;
        for k in 0..8 {
            bits |= (p[ro + 2 + k] as u64) << (8 * k);
        }
        out.push(OccupancyRecord { word, bits });
    }
    out
}

/// Live 32³ occupancy as a `word -> bits` map. Fold extended frames in sequence
/// order with [`Occupancy::apply`]: each frame overwrites the words it carries,
/// so bits can turn **on or off** (a word set to 0 clears its voxels).
#[derive(Debug, Default, Clone)]
pub struct Occupancy {
    words: BTreeMap<u16, u64>,
}

impl Occupancy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one frame's extended records (no-op for non-extended frames).
    pub fn apply(&mut self, frame: &Frame) {
        for r in decode_extended(frame) {
            if r.bits == 0 {
                self.words.remove(&r.word);
            } else {
                self.words.insert(r.word, r.bits);
            }
        }
    }

    /// Number of occupied voxels in the current state.
    pub fn count(&self) -> usize {
        self.words.values().map(|w| w.count_ones() as usize).sum()
    }

    /// Occupied voxels as `(x, y, z)`, each 0..31, in deterministic order.
    pub fn voxels(&self) -> Vec<(u8, u8, u8)> {
        let mut out = Vec::with_capacity(self.count());
        for (&word, &bits) in &self.words {
            let mut b = bits;
            while b != 0 {
                let bit = b.trailing_zeros();
                out.push(morton_decode3(((word as u32) << 6) | bit));
                b &= b - 1;
            }
        }
        out
    }
}

/// World position of a voxel: `detonate + (v - 16) * 20` per axis.
///
/// This uses the engine's grid convention directly. Note: some 2-D top-down
/// (radar) overlays require the X axis to be negated to match the target
/// image's handedness — see `docs/smoke-voxel-decoding.md`.
pub fn voxel_to_world(v: (u8, u8, u8), detonate: [f32; 3]) -> [f32; 3] {
    [
        (v.0 as f32 - GRID_CENTRE) * VOXEL_SIZE + detonate[0],
        (v.1 as f32 - GRID_CENTRE) * VOXEL_SIZE + detonate[1],
        (v.2 as f32 - GRID_CENTRE) * VOXEL_SIZE + detonate[2],
    ]
}

/// Pack a voxel `(x, y, z)` (each 0..31) into a 15-bit column code
/// `(x << 10) | (y << 5) | z`. This is **not** a Morton code — it decodes with
/// three shifts, so a downstream consumer needs no de-interleave logic.
#[inline]
pub fn pack_voxel(x: u8, y: u8, z: u8) -> u16 {
    ((x as u16) << 10) | ((y as u16) << 5) | (z as u16)
}

/// Inverse of [`pack_voxel`].
#[inline]
pub fn unpack_voxel(code: u16) -> (u8, u8, u8) {
    (((code >> 10) & 31) as u8, ((code >> 5) & 31) as u8, (code & 31) as u8)
}

/// One extended frame's occupancy change vs the previous extended frame, as
/// packed column codes (see [`pack_voxel`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoxelDelta {
    /// Byte offset of the end of the source frame. Anchor to a game tick via
    /// `m_nVoxelFrameDataSize` sampled over ticks (first tick whose cumulative
    /// size ≥ `end_offset`).
    pub end_offset: usize,
    /// Voxels turned ON this frame.
    pub added: Vec<u16>,
    /// Voxels turned OFF this frame (the rare collision-conformance voids).
    pub removed: Vec<u16>,
}

/// Decode the journal into per-extended-frame occupancy **deltas** (added /
/// removed voxels vs the previous frame), preserving the native frame cadence
/// (which is dense during the early bloom and sparse once the volume settles).
/// Frames that change nothing are skipped. Accumulating `added` minus `removed`
/// in order losslessly reproduces each frame's occupancy.
///
/// This is the compact, consumer-ready form intended for a per-round binary:
/// decode here, on the parse machine, so the consumer only accumulates packed
/// coordinates (no Morton/bitset logic). Pair each delta's `end_offset` with the
/// per-tick `m_nVoxelFrameDataSize` series to place it on the game-tick timeline.
pub fn delta_frames(blob: &[u8], size: usize) -> Vec<VoxelDelta> {
    let mut occ = Occupancy::new();
    let mut prev: BTreeSet<u16> = BTreeSet::new();
    let mut out = Vec::new();
    for f in &split_frames(blob, size) {
        if f.payload.get(1).copied().unwrap_or(0) & 2 == 0 {
            continue; // only extended frames mutate occupancy
        }
        occ.apply(f);
        let cur: BTreeSet<u16> = occ.voxels().into_iter().map(|(x, y, z)| pack_voxel(x, y, z)).collect();
        let added: Vec<u16> = cur.difference(&prev).copied().collect();
        let removed: Vec<u16> = prev.difference(&cur).copied().collect();
        prev = cur;
        if added.is_empty() && removed.is_empty() {
            continue; // re-sent word with no net change — nothing to emit
        }
        out.push(VoxelDelta { end_offset: f.end_offset, added, removed });
    }
    out
}

/// Fully decoded smoke geometry.
#[derive(Debug, Clone, Default)]
pub struct DecodedSmoke {
    /// Coarse 2-D footprint lattice from the seed frame (empty if none).
    pub seed_footprint: Vec<SeedPoint>,
    /// Final (fully-bloomed) occupancy voxel set, `(x, y, z)` each 0..31.
    pub final_occupancy: Vec<(u8, u8, u8)>,
    /// Number of extended (occupancy-bearing) frames folded.
    pub extended_frames: usize,
}

/// Decode a smoke journal blob into its seed footprint and final occupancy.
pub fn decode(blob: &[u8], size: usize) -> DecodedSmoke {
    let frames = split_frames(blob, size);
    let mut occ = Occupancy::new();
    let mut seed_footprint = Vec::new();
    let mut extended_frames = 0usize;
    for f in &frames {
        if f.kind == FrameKind::Seed {
            seed_footprint = decode_seed(f.payload);
        }
        if (f.payload.get(1).copied().unwrap_or(0)) & 2 != 0 {
            occ.apply(f);
            extended_frames += 1;
        }
    }
    DecodedSmoke { seed_footprint, final_occupancy: occ.voxels(), extended_frames }
}

impl SmokeRecord {
    /// Decode this smoke's voxel journal into geometry (seed footprint + final
    /// occupancy). For a structurally-invalid journal (see
    /// [`SmokeRecord::voxel_frame_data`]) the occupancy will be empty.
    pub fn decode_voxels(&self) -> DecodedSmoke {
        decode(&self.voxel_frame_data, self.voxel_frame_data_size.max(0) as usize)
    }

    /// Final occupancy voxels mapped to world space via [`voxel_to_world`].
    pub fn occupancy_world(&self) -> Vec<[f32; 3]> {
        self.decode_voxels()
            .final_occupancy
            .into_iter()
            .map(|v| voxel_to_world(v, self.detonate_pos))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference Morton encode used only by the tests.
    fn morton_encode3(x: u8, y: u8, z: u8) -> u32 {
        let mut m = 0u32;
        for i in 0..5 {
            m |= (((x as u32 >> i) & 1) << (3 * i)) as u32;
            m |= (((y as u32 >> i) & 1) << (3 * i + 1)) as u32;
            m |= (((z as u32 >> i) & 1) << (3 * i + 2)) as u32;
        }
        m
    }

    #[test]
    fn morton_round_trips() {
        for &(x, y, z) in &[(0, 0, 0), (31, 31, 31), (16, 16, 16), (1, 2, 3), (17, 5, 30), (10, 21, 8)] {
            assert_eq!(morton_decode3(morton_encode3(x, y, z)), (x, y, z));
        }
    }

    /// Build a `[u16 seq][u16 len][payload]` framed blob from raw payloads.
    fn frame_blob(payloads: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            out.extend_from_slice(&(i as u16).to_le_bytes());
            out.extend_from_slice(&(p.len() as u16).to_le_bytes());
            out.extend_from_slice(p);
        }
        out
    }

    /// One extended payload carrying the given (word, bits) records.
    fn extended_payload(records: &[(u16, u64)]) -> Vec<u8> {
        let mut p = vec![0u8, 2u8]; // byte0, sectionFlags=2
        p.extend_from_slice(&(records.len() as u16).to_le_bytes());
        for &(word, bits) in records {
            p.extend_from_slice(&word.to_le_bytes());
            p.extend_from_slice(&bits.to_le_bytes());
        }
        p.push(0); // trailing byte
        p
    }

    fn voxel_record(x: u8, y: u8, z: u8) -> (u16, u64) {
        let m = morton_encode3(x, y, z);
        ((m >> 6) as u16, 1u64 << (m & 63))
    }

    #[test]
    fn classifies_frames() {
        let heartbeat = vec![0u8, 0, 0];
        let other = vec![1u8, 0, 0];
        let extended = extended_payload(&[voxel_record(16, 16, 16)]);
        let blob = frame_blob(&[heartbeat, other, extended]);
        let frames = split_frames(&blob, blob.len());
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].kind, FrameKind::Heartbeat);
        assert_eq!(frames[1].kind, FrameKind::Other);
        assert_eq!(frames[2].kind, FrameKind::Extended);
        assert_eq!(frames[2].end_offset, blob.len());
    }

    #[test]
    fn decodes_extended_occupancy() {
        let want = [(16u8, 16, 16), (0, 0, 0), (31, 31, 31), (5, 9, 20)];
        let payload = extended_payload(&want.iter().map(|&(x, y, z)| voxel_record(x, y, z)).collect::<Vec<_>>());
        let blob = frame_blob(&[payload]);
        let frames = split_frames(&blob, blob.len());
        let mut occ = Occupancy::new();
        occ.apply(&frames[0]);
        let mut got = occ.voxels();
        got.sort();
        let mut exp = want.to_vec();
        exp.sort();
        assert_eq!(got, exp);
        assert_eq!(occ.count(), want.len());
    }

    #[test]
    fn extended_overwrite_can_clear() {
        // word folds are overwrite: re-sending a word with fewer bits clears voxels.
        let (w, _) = voxel_record(16, 16, 16);
        let on = extended_payload(&[(w, 0xFFFF_FFFF_FFFF_FFFF)]);
        let off = extended_payload(&[(w, 0)]);
        let blob = frame_blob(&[on, off]);
        let frames = split_frames(&blob, blob.len());
        let mut occ = Occupancy::new();
        occ.apply(&frames[0]);
        assert_eq!(occ.count(), 64);
        occ.apply(&frames[1]);
        assert_eq!(occ.count(), 0);
    }

    #[test]
    fn decodes_seed_footprint() {
        // count=2, records [x,y,z,flag,pad..]
        let mut p = vec![0u8, 1u8, 2u8]; // byte0, sectionFlags=1 (seed), count=2
        p.extend_from_slice(&[10, 11, 16, 5, 0, 0, 0, 0]);
        p.extend_from_slice(&[20, 21, 16, 0, 0, 0, 0, 0]);
        let seed = decode_seed(&p);
        assert_eq!(seed.len(), 2);
        assert_eq!(seed[0], SeedPoint { x: 10, y: 11, z: 16, flag: 5 });
        assert_eq!(seed[1], SeedPoint { x: 20, y: 21, z: 16, flag: 0 });
    }

    #[test]
    fn maps_voxel_to_world() {
        // centre voxel sits exactly on the detonation position.
        assert_eq!(voxel_to_world((16, 16, 16), [100.0, -50.0, 8.0]), [100.0, -50.0, 8.0]);
        // +1 voxel in x == +20u.
        assert_eq!(voxel_to_world((17, 16, 16), [0.0, 0.0, 0.0]), [20.0, 0.0, 0.0]);
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(x, y, z) in &[(0u8, 0u8, 0u8), (31, 31, 31), (16, 16, 16), (17, 5, 30)] {
            assert_eq!(unpack_voxel(pack_voxel(x, y, z)), (x, y, z));
        }
    }

    #[test]
    fn delta_frames_add_then_remove() {
        // Two voxels in the SAME word so one frame can clear one of them.
        let w = 5u16;
        let v0 = morton_decode3(((w as u32) << 6) | 0);
        let v1 = morton_decode3(((w as u32) << 6) | 1);
        let p0 = pack_voxel(v0.0, v0.1, v0.2);
        let p1 = pack_voxel(v1.0, v1.1, v1.2);
        // frame A: word 5 = bits {0,1} (adds v0,v1); frame B: word 5 = {1} (removes v0)
        let blob = frame_blob(&[extended_payload(&[(w, 0b11)]), extended_payload(&[(w, 0b10)])]);
        let d = delta_frames(&blob, blob.len());
        assert_eq!(d.len(), 2);
        let mut a0 = d[0].added.clone();
        a0.sort();
        let mut exp = vec![p0, p1];
        exp.sort();
        assert_eq!(a0, exp);
        assert!(d[0].removed.is_empty());
        assert!(d[1].added.is_empty());
        assert_eq!(d[1].removed, vec![p0]);
        assert_eq!(d[1].end_offset, blob.len());
    }

    #[test]
    fn delta_frames_skip_noop_and_reproduce_final() {
        let want = [(16u8, 16, 16), (0, 0, 0), (31, 31, 31), (5, 9, 20), (8, 8, 8)];
        let f1 = extended_payload(&want[..3].iter().map(|&(x, y, z)| voxel_record(x, y, z)).collect::<Vec<_>>());
        let f1_again = f1.clone(); // identical re-send -> no net change -> skipped
        let f2 = extended_payload(&want[3..].iter().map(|&(x, y, z)| voxel_record(x, y, z)).collect::<Vec<_>>());
        let blob = frame_blob(&[f1, f1_again, f2]);
        let deltas = delta_frames(&blob, blob.len());
        assert_eq!(deltas.len(), 2, "the identical re-send frame must be skipped");
        // accumulate added - removed -> must equal the full occupancy
        let mut live: BTreeSet<u16> = BTreeSet::new();
        for d in &deltas {
            for &c in &d.added { live.insert(c); }
            for &c in &d.removed { live.remove(&c); }
        }
        let mut got: Vec<(u8, u8, u8)> = live.iter().map(|&c| unpack_voxel(c)).collect();
        got.sort();
        let mut exp = want.to_vec();
        exp.sort();
        assert_eq!(got, exp);
    }
}
