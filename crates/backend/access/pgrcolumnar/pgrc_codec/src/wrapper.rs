//! Wrapper block assembly (spec §6.4; delegated to M3-C by spec §20).
//!
//! A wrapped stream section (`wrapper != 0` in the header) replaces the
//! plain payload region with **per-granule compressed blocks** prefixed by a
//! `u32 × (granule_count + 1)` block-offset table; the frame table then
//! addresses the *uncompressed* payload image and `uncompressed_len` sizes
//! the scratch. Wrappers carry DISK-ONLY semantics (the two-layer law,
//! O-CMP-3(a), ruled 2026-08-10): the offer extends to every election — hot
//! fixed-width included — and wrapped extents decompress to the ENCODED
//! image at extent open (once per part residency, O-CMP-5(a)), so the hot
//! kernels stay wrapper-oblivious and allocation-free and the in-memory
//! tier never sees a wrapper.
//!
//! - [`wrap_section`] rewrites an unwrapped section (as emitted by
//!   `StreamSectionWriter`) into the wrapped form, given the per-granule
//!   payload boundaries the encode driver recorded.
//! - [`unwrap_section`] rebuilds the unwrapped section image in a
//!   caller-owned scratch buffer; kernels then run on the scratch exactly as
//!   on an unwrapped section.
//! - LZ4 (wrapper id 1) is the in-repo block codec (`lz4.rs`); Zstd
//!   (wrapper id 2) is the CMP-A slot-fill over the workspace `zstd` dep
//!   per O-CMP-2(a) — per-granule single-frame blocks at a pinned level, so
//!   wrapped bytes stay a pure function of the input bytes (the
//!   byte-identical-parts law, at a pinned library version). On wasm32 the
//!   zstd arm refuses TYPED (`WrapperUnsupported`) both directions — the C
//!   binding has no wasm build (the v1 reader's documented caveat) and
//!   parity-honest degradation beats a silent wrong answer.
//!
//! - The pgrc format 2.2 dictionary layers (spec §6.4/§7): `FrontZstd` (id 3,
//!   `DictPayload`) front-codes each dict frame before its zstd block and
//!   `PackedZstd` (id 4, `DictIndex`) packs the 12 B records before the one
//!   zstd block. Both expand back to the v1 images in the unwrap; the
//!   payload's expand needs the entry lengths, so its unwrap faces take the
//!   (already resident, v1-image) index records — [`unwrap_dict_payload_section`]
//!   / [`unwrap_dict_payload_block`]; the role-agnostic faces refuse it typed.
//!
//! The election (`election.rs`) engages a wrapper only under the
//! wrapper-layer ≥20%-win law (O-CMP-4(a)) against the unwrapped bytes;
//! encodings keep the standing ≥10% law.

use crate::{intpre, lz4};
use pgrc_format::dict::{
    dict_frame_records, expand_front_coded_frame, expand_packed_dict_index, front_code_frame,
    pack_dict_index,
};
use pgrc_format::enc::Wrapper;
use pgrc_format::part::{StreamSectionHdr, STREAM_SECTION_HDR_LEN, STREAM_SECTION_MAGIC};
use pgrc_format::{FormatError, FormatResult};

/// The default zstd compression level for the wrapper layer. Disk-only layer:
/// the election prices the EXACT wrapped bytes, so the level is a size/CPU
/// trade only; decode is level-agnostic, so every build reads every level.
const ZSTD_LEVEL: i32 = 3;

/// The wrapper layer's zstd level: `PGRUST_PGRC_ZSTD_LEVEL=<1..=22>` (read once), else [`ZSTD_LEVEL`]. Part bytes stay a
/// deterministic function of input at a given level.
pub fn zstd_level() -> i32 {
    static L: pgsync::OnceLock<i32> = pgsync::OnceLock::new();
    *L.get_or_init(|| parse_level(std::env::var("PGRUST_PGRC_ZSTD_LEVEL").ok().as_deref()).unwrap_or(ZSTD_LEVEL))
}

/// A zstd level from a switch's text: `1..=22`, else None (unset, junk or out of range).
pub fn parse_level(v: Option<&str>) -> Option<i32> {
    v.and_then(|v| v.trim().parse::<i32>().ok()).filter(|l| (1..=22).contains(l))
}

/// `PGRUST_PGRC_ZSTD_LEVEL_TEXT=<1..=22>` (read once; unset = string data keeps [`zstd_level`]).
pub fn zstd_level_text() -> Option<i32> {
    static L: pgsync::OnceLock<Option<i32>> = pgsync::OnceLock::new();
    *L.get_or_init(|| parse_level(std::env::var("PGRUST_PGRC_ZSTD_LEVEL_TEXT").ok().as_deref()))
}

/// String data: a dictionary payload (front-coded entries, plain or OnPair-tokenized) or a varlena Verbatim value
/// section (width 0).
pub fn is_string_data(wrapper: Wrapper, encoding: u16, width: u8) -> bool {
    matches!(wrapper, Wrapper::FrontZstd | Wrapper::FrontOnPairZstd)
        || (encoding == pgrc_format::enc::EncodingId::Verbatim as u16 && width == 0)
}

/// The level of a section's blocks before any adaptive choice: `text` for string data when set, else `base`.
pub fn level_for(wrapper: Wrapper, encoding: u16, width: u8, base: i32, text: Option<i32>) -> i32 {
    match text {
        Some(t) if is_string_data(wrapper, encoding, width) => t,
        _ => base,
    }
}

/// A section's zstd blocks compress at `high` instead of `low` iff its sample (the first block's zstd input, cut to
/// [`ADAPT_SAMPLE`] bytes) saves at least `permille` thousandths of the unwrapped bytes it stands for ON DISK at
/// `high`: each level's frame counts only if it clears the 20 % wrapper gate, else the bytes stay unwrapped. `gated`
/// also needs the sample to clear the gate at `low`. Sections under [`ADAPT_MIN`] payload bytes take `low` unsampled.
/// The choice reads only the input bytes, so part bytes stay a deterministic function of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZstdAdapt {
    pub low: i32,
    pub high: i32,
    pub permille: u32,
    pub gated: bool,
}

pub const ADAPT_MIN: usize = 16 << 10;
pub const ADAPT_SAMPLE: usize = 64 << 10;

impl ZstdAdapt {
    pub fn high_wins(&self, base: usize, low_len: usize, high_len: usize) -> bool {
        let base = base as u128;
        let disk = |w: usize| if (w as u128) * 5 <= base * 4 { w as u128 } else { base };
        let saved = disk(low_len).saturating_sub(disk(high_len));
        saved > 0 && saved * 1000 >= self.permille as u128 * base && (!self.gated || disk(low_len) < base)
    }
}

/// `PGRUST_PGRC_ZSTD_ADAPTIVE=1` (read once; off = [`zstd_level`] for every block): `_LOW` (default [`zstd_level`]),
/// `_HIGH` (default 6), `_X` = `permille` (default 40), `_GATED=1`.
pub fn zstd_adapt() -> Option<&'static ZstdAdapt> {
    static A: pgsync::OnceLock<Option<ZstdAdapt>> = pgsync::OnceLock::new();
    A.get_or_init(|| {
        let var = |k: &str| std::env::var(k).ok();
        let on = |k: &str| var(k).as_deref().map(str::trim) == Some("1");
        if !on("PGRUST_PGRC_ZSTD_ADAPTIVE") {
            return None;
        }
        let level = |k: &str, d: i32| var(k).and_then(|v| v.trim().parse::<i32>().ok()).filter(|l| (1..=22).contains(l)).unwrap_or(d);
        Some(ZstdAdapt {
            low: level("PGRUST_PGRC_ZSTD_ADAPTIVE_LOW", zstd_level()),
            high: level("PGRUST_PGRC_ZSTD_ADAPTIVE_HIGH", 6),
            permille: var("PGRUST_PGRC_ZSTD_ADAPTIVE_X").and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(40),
            gated: on("PGRUST_PGRC_ZSTD_ADAPTIVE_GATED"),
        })
    })
    .as_ref()
}

/// Whether this build can ENCODE/DECODE the given wrapper arm. The seal
/// election consults this before pricing an arm (a build without the codec
/// never elects it — parity-honest degradation); the decode side needs no
/// probe because [`unwrap_section`] refuses typed on the missing arm.
pub const fn wrapper_available(w: Wrapper) -> bool {
    match w {
        Wrapper::None => false,
        Wrapper::Lz4 => true,
        Wrapper::Zstd | Wrapper::FrontZstd | Wrapper::PackedZstd | Wrapper::FrontOnPairZstd | Wrapper::OnPairZstd | Wrapper::IntPre => {
            cfg!(not(target_family = "wasm"))
        }
    }
}

/// Compress one granule block under `wrapper` (zstd at `level`), appended to `out`.
fn compress_block(wrapper: Wrapper, input: &[u8], out: &mut Vec<u8>, level: i32) -> FormatResult<()> {
    match wrapper {
        Wrapper::Lz4 => {
            lz4::compress(input, out);
            Ok(())
        }
        Wrapper::Zstd | Wrapper::FrontZstd | Wrapper::PackedZstd => zstd_compress_block_at(input, out, level),
        Wrapper::None | Wrapper::FrontOnPairZstd | Wrapper::OnPairZstd | Wrapper::IntPre => Err(FormatError::EncodeContract {
            detail: "compress_block with Wrapper::None, an OnPair arm or IntPre",
        }),
    }
}

/// Inflate one zstd block whose decoded size is not known in advance
/// (the dict layers' intermediate images) into a transient buffer bounded
/// by `cap`; a frame past the cap is typed corruption.
fn zstd_inflate_bounded(src: &[u8], cap: usize) -> FormatResult<Vec<u8>> {
    let mut buf = pgrc_format::try_filled(cap, 0u8, "wrapper")?;
    let n = zstd_decompress_block(src, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(not(target_family = "wasm"))]
pgsync::process_global! {
    /// C1-REG: zstd compression contexts kept for the next call instead of created and freed per block (a fresh
    /// context allocates its workspace, and on an rtpool worker that churn is memory the allocators keep). At most
    /// [`ZSTD_IDLE_MAX`] idle contexts are held (one per concurrent caller is enough; a surplus one is freed as
    /// before). A kept context writes the same frame a fresh one does: `ZSTD_compress2` resets the session and takes
    /// the frame's parameters from the level alone, the only parameter these contexts carry, kept beside each
    /// context (`zstd_compress_block_matches_a_fresh_context`).
    static ZSTD_IDLE: pgsync::Mutex<Vec<(zstd::bulk::Compressor<'static>, i32)>> = pgsync::Mutex::new(Vec::new());
}
#[cfg(not(target_family = "wasm"))]
const ZSTD_IDLE_MAX: usize = 8;

pub(crate) fn zstd_compress_block(input: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    zstd_compress_block_at(input, out, zstd_level())
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn zstd_compress_block_at(input: &[u8], out: &mut Vec<u8>, level: i32) -> FormatResult<()> {
    // One zstd frame per granule block (`ZSTD_compress2`: content size stamped
    // in the frame header; deterministic at a pinned level + library),
    // written straight after `out`'s bytes (no frame buffer of its own).
    let err = |_| FormatError::EncodeContract {
        detail: "zstd compress",
    };
    let idle = pgsync::lock(&ZSTD_IDLE).pop();
    let (mut cctx, mut at) = match idle {
        Some(c) => c,
        None => (zstd::bulk::Compressor::new(level).map_err(err)?, level),
    };
    if at != level {
        cctx.set_compression_level(level).map_err(err)?;
        at = level;
    }
    pgrc_format::try_reserve_more(out, zstd_bound(input.len()), "pgrc zstd block")?;
    let start = out.len();
    let mut dst = std::io::Cursor::new(&mut *out);
    dst.set_position(start as u64);
    let r = cctx.compress_to_buffer(input, &mut dst);
    let mut idle = pgsync::lock(&ZSTD_IDLE);
    if idle.len() < ZSTD_IDLE_MAX {
        idle.push((cctx, at));
    }
    drop(idle);
    r.map_err(err)?;
    Ok(())
}

#[cfg(target_family = "wasm")]
pub(crate) fn zstd_compress_block_at(_input: &[u8], _out: &mut Vec<u8>, _level: i32) -> FormatResult<()> {
    Err(FormatError::WrapperUnsupported {
        wrapper: Wrapper::Zstd.as_u8(),
    })
}

/// Decompress one zstd granule block into the exact remaining image span;
/// returns the decoded length. Bounds-validated by the zstd frame machinery
/// (a frame larger than `dst` errors); typed errors, never a panic.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn zstd_decompress_block(src: &[u8], dst: &mut [u8]) -> FormatResult<usize> {
    // D4632: a pooled decompression context (`zpool`), not one created and freed per block
    let r = if zpool::on() { zpool::decompress(src, dst) } else { zstd::bulk::decompress_to_buffer(src, dst) };
    r.map_err(|_| FormatError::Corrupt { at: "zstd block" })
}

/// D4632: the zstd contexts, kept process-wide and reused. zstd-sys allocates them with C malloc (glibc), outside every
/// account. Before: `bulk::decompress_to_buffer` created and freed a DCtx per granule block (220,767 a fast drift leg),
/// and each thread's compressor freed its ~1 MiB CCtx workspace when the thread ended; glibc's arenas kept the freed
/// bytes (G3n +0.85..1.32 MiB a round with glibc's live bytes flat: alloclint's trace, D4632 NOTE). Now a context is
/// taken from the pool for one block and put back (a pool lock per block, beside a block's decompress or compress), so
/// the contexts allocated are about the peak number of threads using zstd at once, and none is freed while the pool
/// keeps it. At most `KEEP` of each kind are kept; a context put back past that is freed.
/// `PGRUST_ZSTD_POOL=0`: off (the old per-block DCtx and per-thread CCtx), the A/B switch.
#[cfg(not(target_family = "wasm"))]
mod zpool {
    use pgsync::Mutex;

    /// The most contexts of a kind the pool keeps (a DCtx is ~100 KiB, a level-3 CCtx ~1 MiB once used): fixed slots, so
    /// the pool itself never allocates.
    const KEEP: usize = 64;

    static DCTX: Mutex<[Option<zstd::bulk::Decompressor<'static>>; KEEP]> = Mutex::new([const { None }; KEEP]);
    static CCTX: Mutex<[Option<zstd::bulk::Compressor<'static>>; KEEP]> = Mutex::new([const { None }; KEEP]);

    pub(super) fn on() -> bool {
        static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
        *ON.get_or_init(|| !std::env::var("PGRUST_ZSTD_POOL").is_ok_and(|v| v.trim() == "0"))
    }

    fn take<T>(pool: &Mutex<[Option<T>; KEEP]>) -> Option<T> {
        pgsync::lock(pool).iter_mut().find_map(Option::take)
    }

    /// Back into a free slot; with every slot taken the context is freed.
    fn put<T>(pool: &Mutex<[Option<T>; KEEP]>, c: T) {
        if let Some(s) = pgsync::lock(pool).iter_mut().find(|s| s.is_none()) {
            *s = Some(c);
        }
    }

    pub(super) fn decompress(src: &[u8], dst: &mut [u8]) -> std::io::Result<usize> {
        let taken = take(&DCTX);
        let mut d = match taken {
            Some(d) => d,
            None => zstd::bulk::Decompressor::new()?,
        };
        let r = d.decompress_to_buffer(src, dst);
        // a context that failed a frame is not put back (the next block starts from a fresh one)
        if r.is_ok() {
            put(&DCTX, d);
        }
        r
    }

    pub(super) fn compress(input: &[u8]) -> std::io::Result<Vec<u8>> {
        let taken = take(&CCTX);
        let mut c = match taken {
            Some(c) => c,
            None => zstd::bulk::Compressor::new(super::zstd_level())?,
        };
        let r = c.compress(input);
        if r.is_ok() {
            put(&CCTX, c);
        }
        r
    }
}

#[cfg(target_family = "wasm")]
pub(crate) fn zstd_decompress_block(_src: &[u8], _dst: &mut [u8]) -> FormatResult<usize> {
    Err(FormatError::WrapperUnsupported {
        wrapper: Wrapper::Zstd.as_u8(),
    })
}

/// Byte length of the wrapped form of `section` (exact), or None when the
/// wrapper loses (never smaller). The election consumes this.
pub fn wrapped_len(
    section: &[u8],
    granule_payload_ends: &[u32],
    wrapper: Wrapper,
) -> FormatResult<usize> {
    let mut out = Vec::new();
    wrap_section(section, granule_payload_ends, wrapper, &mut out)?;
    Ok(out.len())
}

/// The most bytes [`wrap_section`] can append for `section` cut at `granule_payload_ends` under `wrapper`: the section
/// itself, the block-offset table, and every block's worst-case growth under the codec (C1-REG). A caller that reserves
/// this much once never has the wrap reallocate `out`, so a pool worker can fill a buffer its caller allocated (and
/// frees on its own thread).
pub fn wrap_section_bound(section: &[u8], granule_payload_ends: &[u32], wrapper: Wrapper) -> usize {
    let mut g0 = 0usize;
    let growth: usize = granule_payload_ends
        .iter()
        .map(|&end| {
            let n = (end as usize).saturating_sub(g0);
            g0 = end as usize;
            block_bound(wrapper, n) - n
        })
        .sum();
    section.len() + (granule_payload_ends.len() + 1) * 4 + growth
}

/// The most bytes [`compress_block`] appends for an `n`-byte block: zstd's `ZSTD_compressBound`, lz4's
/// `n + n / 255 + 16` (the all-literals sequence: one token, the length bytes, the literals).
fn block_bound(wrapper: Wrapper, n: usize) -> usize {
    match wrapper {
        Wrapper::Lz4 => n + n / 255 + 16,
        Wrapper::IntPre => 1 + zstd_bound(n),
        _ => zstd_bound(n),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn zstd_bound(n: usize) -> usize {
    zstd::zstd_safe::compress_bound(n)
}

#[cfg(target_family = "wasm")]
pub(crate) fn zstd_bound(n: usize) -> usize {
    n
}

/// Rewrite an UNWRAPPED section into the wrapped form (spec §6.4).
///
/// `granule_payload_ends[g]` = payload-relative end offset of granule `g`'s
/// payload bytes (ascending; last entry == payload image length). The frame
/// table and gcount table are carried verbatim — their offsets address the
/// uncompressed image by spec.
pub fn wrap_section(
    section: &[u8],
    granule_payload_ends: &[u32],
    wrapper: Wrapper,
    out: &mut Vec<u8>,
) -> FormatResult<()> {
    match wrapper {
        Wrapper::Lz4 | Wrapper::Zstd | Wrapper::IntPre => {}
        Wrapper::None | Wrapper::FrontZstd | Wrapper::PackedZstd | Wrapper::FrontOnPairZstd | Wrapper::OnPairZstd => {
            return Err(FormatError::EncodeContract {
                detail: "wrap_section with a role-bound or absent wrapper",
            })
        }
    }
    wrap_section_with(section, granule_payload_ends, wrapper, out, None, zstd_adapt(), zstd_level_text()).map(|_| ())
}

/// `Wrapper::FrontZstd` over an UNWRAPPED DictPayload section: one block
/// per dict frame (the frame table is the block geometry), each frame
/// front-coded against `index` (the v1 12 B records of the whole
/// dictionary) and then zstd-compressed.
pub fn wrap_dict_payload_section(section: &[u8], index: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    let hdr = StreamSectionHdr::decode(section)?;
    let ends = frame_block_ends(&hdr, section)?;
    wrap_section_with(section, &ends, Wrapper::FrontZstd, out, Some(&mut |b, raw, blk| {
        front_code_frame(raw, dict_frame_records(index, b as u32)?, blk)
    }), zstd_adapt(), zstd_level_text())
    .map(|_| ())
}

/// `Wrapper::PackedZstd` over an UNWRAPPED DictIndex section: the packed
/// image in one zstd block.
pub fn wrap_dict_index_section(section: &[u8], out: &mut Vec<u8>) -> FormatResult<()> {
    let hdr = StreamSectionHdr::decode(section)?;
    let ends = frame_block_ends(&hdr, section)?;
    if ends.len() != 1 {
        return Err(FormatError::EncodeContract { detail: "framed dict index" });
    }
    wrap_section_with(section, &ends, Wrapper::PackedZstd, out, Some(&mut |_b, raw, blk| {
        pack_dict_index(raw, blk)
    }), zstd_adapt(), zstd_level_text())
    .map(|_| ())
}

/// Block ends of a byte-run section: one per frame when framed, else the
/// whole payload (the SB-7 geometry `seal.rs` cuts).
pub(crate) fn frame_block_ends(hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u32>> {
    let payload_end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    };
    let payload_len = payload_end
        .checked_sub(STREAM_SECTION_HDR_LEN)
        .ok_or(FormatError::Bounds { at: "payload region" })? as u32;
    Ok(match hdr.frame_table(section)? {
        Some(offs) if !offs.is_empty() => offs
            .iter()
            .skip(1)
            .copied()
            .chain(core::iter::once(payload_len))
            .collect(),
        _ => vec![payload_len],
    })
}

/// Returns the zstd level the blocks were compressed at. String data under a `text` level skips `adapt`.
pub(crate) fn wrap_section_with(
    section: &[u8],
    granule_payload_ends: &[u32],
    wrapper: Wrapper,
    out: &mut Vec<u8>,
    mut transform: Option<&mut dyn FnMut(usize, &[u8], &mut Vec<u8>) -> FormatResult<()>>,
    adapt: Option<&ZstdAdapt>,
    text: Option<i32>,
) -> FormatResult<i32> {
    let hdr = StreamSectionHdr::decode(section)?;
    if hdr.wrapper != Wrapper::None.as_u8() {
        return Err(FormatError::EncodeContract {
            detail: "wrap_section input already wrapped",
        });
    }
    let payload_end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    };
    if payload_end < STREAM_SECTION_HDR_LEN || payload_end > section.len() {
        return Err(FormatError::Bounds {
            at: "payload region",
        });
    }
    let payload = &section[STREAM_SECTION_HDR_LEN..payload_end];
    let tables = &section[payload_end..];
    let nblocks = granule_payload_ends.len();
    if nblocks == 0
        || granule_payload_ends[nblocks - 1] as usize != payload.len()
        || granule_payload_ends.windows(2).any(|w| w[0] > w[1])
    {
        return Err(FormatError::EncodeContract {
            detail: "granule payload boundaries",
        });
    }

    let mut level = level_for(wrapper, hdr.encoding, hdr.width, zstd_level(), text);
    let texted = text.is_some() && is_string_data(wrapper, hdr.encoding, hdr.width);
    let mut first: Option<Vec<u8>> = None;
    if let Some(a) = adapt.filter(|_| wrapper != Wrapper::Lz4 && !texted) {
        level = a.low;
        if payload.len() >= ADAPT_MIN {
            let raw0 = &payload[..granule_payload_ends[0] as usize];
            let mut b0: Vec<u8> = Vec::new();
            let src: &[u8] = match transform.as_mut() {
                Some(t) => {
                    t(0, raw0, &mut b0)?;
                    &b0
                }
                None => raw0,
            };
            let sample = &src[..src.len().min(ADAPT_SAMPLE)];
            let (mut lo, mut hi) = (Vec::new(), Vec::new());
            zstd_compress_block_at(sample, &mut lo, a.low)?;
            zstd_compress_block_at(sample, &mut hi, a.high)?;
            let base = (sample.len() as u128 * raw0.len() as u128 / src.len().max(1) as u128) as usize;
            if a.high_wins(base, lo.len(), hi.len()) {
                level = a.high;
            }
            if sample.len() == src.len() && wrapper != Wrapper::IntPre {
                first = Some(if level == a.high { hi } else { lo });
            }
        }
    }

    let start = out.len();
    out.resize(start + STREAM_SECTION_HDR_LEN, 0);
    // Block-offset table placeholder: nblocks + 1 u32s, payload-relative.
    let btab_off = out.len();
    out.resize(btab_off + (nblocks + 1) * 4, 0);
    // The payload region (block table + blocks) starts right after the
    // header; block offsets are payload-relative per spec §6.4.
    let payload_region_start = btab_off;
    let mut block_offs: Vec<u32> = Vec::with_capacity(nblocks + 1);
    block_offs.push((out.len() - payload_region_start) as u32);
    let mut g0 = 0usize;
    let mut blk: Vec<u8> = Vec::new();
    let view = intpre::view_for(hdr.encoding, hdr.width);
    let mut trial = intpre::Trial::default();
    for (b, &end) in granule_payload_ends.iter().enumerate() {
        let raw = &payload[g0..end as usize];
        // No transform (a plain wrap): the raw block compresses in place (C1-REG: no per-block copy).
        match (first.take(), transform.as_mut()) {
            (Some(f), _) => {
                pgrc_format::try_reserve_more(out, f.len(), "pgrc zstd block")?;
                out.extend_from_slice(&f);
            }
            (None, Some(t)) => {
                blk.clear();
                t(b, raw, &mut blk)?;
                compress_block(wrapper, &blk, out, level)?;
            }
            (None, None) if wrapper == Wrapper::IntPre => {
                intpre::wrap_block(view, raw, out, &mut trial, level)?;
            }
            (None, None) => compress_block(wrapper, raw, out, level)?,
        }
        block_offs.push((out.len() - payload_region_start) as u32);
        g0 = end as usize;
    }
    for (i, &o) in block_offs.iter().enumerate() {
        out[btab_off + i * 4..btab_off + i * 4 + 4].copy_from_slice(&o.to_le_bytes());
    }
    let frame_table_off = if hdr.frame_table_off != 0 {
        (out.len() - start) as u32
    } else {
        0
    };
    // Frame table then gcount table were contiguous in the source; copy the
    // whole table region and recompute both offsets by their relative order.
    let gcount_delta = if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off - payload_end as u32
    } else {
        0
    };
    let tables_start = (out.len() - start) as u32;
    out.extend_from_slice(tables);
    let gcount_table_off = if hdr.gcount_table_off != 0 {
        tables_start + gcount_delta
    } else {
        0
    };

    // Header: same frozen field order as `StreamSectionHdr` (spec §6.4).
    let h = &mut out[start..start + STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = wrapper.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[12..16].copy_from_slice(&frame_table_off.to_le_bytes());
    h[16..20].copy_from_slice(&gcount_table_off.to_le_bytes());
    h[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    h[28..32].copy_from_slice(&0u32.to_le_bytes());
    Ok(level)
}

/// The largest zero fill or copy an unwrap makes between two asks of its `stop` (cancelpoll's `PIECE`, 1 MiB: about 18 ms
/// of first-touch page faults under memory pressure).
pub const FILL_PIECE: usize = 1 << 20;

/// Rebuild the unwrapped section image from a wrapped one into `scratch`
/// (cleared first). After this, `scratch` is byte-usable by every kernel as
/// `KernelCtx::bytes`. The wrapped section's CRC was already validated by
/// the reader (spec §5.2); the rebuilt image needs no second CRC.
pub fn unwrap_section(section: &[u8], scratch: &mut Vec<u8>) -> FormatResult<()> {
    unwrap_section_with(section, None, scratch, &mut || false).map(drop)
}

/// D3887: [`unwrap_section`] asking `stop` before each block: `Ok(false)` when it said stop, `scratch` then holding no
/// image (a dictionary payload under a role-agnostic wrapper).
pub fn unwrap_section_until(section: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    unwrap_section_with(section, None, scratch, stop)
}

/// The DictPayload unwrap: `index` is the dictionary's v1 index image
/// (12 B records), which a `FrontZstd` section needs for its expand;
/// every other wrapper ignores it.
pub fn unwrap_dict_payload_section(section: &[u8], index: &[u8], scratch: &mut Vec<u8>) -> FormatResult<()> {
    unwrap_section_with(section, Some(index), scratch, &mut || false).map(drop)
}

/// D3887: [`unwrap_dict_payload_section`] asking `stop` before each block (a large dictionary's payload is hundreds of
/// blocks and seconds of inflate and expand): `Ok(false)` when it said stop, `scratch` then holding no image.
pub fn unwrap_dict_payload_section_until(section: &[u8], index: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    unwrap_section_with(section, Some(index), scratch, stop)
}

/// [D302] A DictPayload header's `uncompressed_len` against the writer's law over the
/// CRC-validated v1 `index` ([`pgrc_format::dict::dict_payload_image_len_range`]): the
/// image ends 0..=7 bytes past the last entry the index records. Every dict arm applies it
/// before any image-sized reservation (the reader's block-lazy arm too), so the arms agree on
/// which payloads are readable; a header the index contradicts is typed corruption.
pub fn dict_payload_len_checked(hdr: &StreamSectionHdr, index: &[u8]) -> FormatResult<()> {
    let (lo, hi) = pgrc_format::dict::dict_payload_image_len_range(index)?;
    let unc = hdr.uncompressed_len as u64;
    if unc < lo || unc > hi {
        return Err(FormatError::Corrupt {
            at: "dict payload uncompressed_len vs dict index",
        });
    }
    Ok(())
}

fn unwrap_section_with(section: &[u8], index: Option<&[u8]>, scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    let hdr = StreamSectionHdr::decode(section)?;
    let wrapper = Wrapper::from_u8(hdr.wrapper)?;
    match wrapper {
        Wrapper::Lz4 | Wrapper::Zstd | Wrapper::PackedZstd | Wrapper::IntPre => {}
        Wrapper::FrontZstd if index.is_some() => {}
        Wrapper::FrontZstd => {
            return Err(FormatError::Corrupt {
                at: "front-coded dict payload unwrap without the dict index",
            })
        }
        Wrapper::FrontOnPairZstd => {
            let index = index.ok_or(FormatError::Corrupt { at: "front-coded dict payload unwrap without the dict index" })?;
            return crate::onpairwrap::unwrap_dict_payload(&hdr, section, index, scratch, stop);
        }
        Wrapper::OnPairZstd => return crate::onpairvals::unwrap_values(&hdr, section, scratch, stop),
        Wrapper::None => {
            return Err(FormatError::Corrupt {
                at: "unwrap of unwrapped section",
            })
        }
    }
    let payload_end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    };
    if payload_end < STREAM_SECTION_HDR_LEN || payload_end > section.len() {
        return Err(FormatError::Bounds {
            at: "payload region",
        });
    }
    let wrapped_payload = &section[STREAM_SECTION_HDR_LEN..payload_end];
    let tables = &section[payload_end..];

    // Block-offset table: u32 × (nblocks + 1), payload-relative; the first
    // entry is the table's own length, which yields the block count.
    if wrapped_payload.len() < 4 {
        return Err(FormatError::Truncated {
            at: "wrapper block table",
        });
    }
    let first = u32::from_le_bytes(wrapped_payload[..4].try_into().expect("len 4")) as usize;
    if first < 4 || first % 4 != 0 || first > wrapped_payload.len() {
        return Err(FormatError::Corrupt {
            at: "wrapper block table",
        });
    }
    let nblocks = first / 4 - 1;
    let mut offs = Vec::with_capacity(nblocks + 1);
    for i in 0..=nblocks {
        let b = wrapped_payload
            .get(i * 4..i * 4 + 4)
            .ok_or(FormatError::Truncated {
                at: "wrapper block table",
            })?;
        offs.push(u32::from_le_bytes(b.try_into().expect("len 4")) as usize);
    }
    if offs.windows(2).any(|w| w[0] > w[1])
        || *offs.last().expect("nonempty") > wrapped_payload.len()
    {
        return Err(FormatError::Corrupt {
            at: "wrapper block offsets",
        });
    }

    // [D302] A dict payload's declared image answers to its index before
    // the reservation; every reservation is fallible (typed refusal, never
    // `handle_alloc_error`'s abort): `uncompressed_len` is a file-controlled
    // u32, up to ~4 GiB.
    if let Some(index) = index {
        dict_payload_len_checked(&hdr, index)?;
    }
    let need = STREAM_SECTION_HDR_LEN + hdr.uncompressed_len as usize + tables.len();
    // D4228: the image's zero fill FILL_PIECE bytes at a time, `stop` asked before each piece past the first: a fill
    // faults in every page of the image (hundreds of ms for a large one under memory pressure) before the first block's ask
    scratch.clear();
    scratch.try_reserve_exact(need).map_err(|_| FormatError::Bounds {
        at: "unwrapped section reservation",
    })?;
    while scratch.len() < need {
        scratch.resize(need.min(scratch.len() + FILL_PIECE), 0);
        if scratch.len() < need && stop() {
            scratch.clear();
            return Ok(false);
        }
    }
    // Header: unwrapped twin.
    let frame_table_off = if hdr.frame_table_off != 0 {
        STREAM_SECTION_HDR_LEN as u32 + hdr.uncompressed_len
    } else {
        0
    };
    let gcount_table_off = if hdr.gcount_table_off != 0 {
        let tables_start = if hdr.frame_table_off != 0 {
            hdr.frame_table_off
        } else {
            hdr.gcount_table_off
        };
        STREAM_SECTION_HDR_LEN as u32 + hdr.uncompressed_len + (hdr.gcount_table_off - tables_start)
    } else {
        0
    };
    let h = &mut scratch[..STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = Wrapper::None.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[12..16].copy_from_slice(&frame_table_off.to_le_bytes());
    h[16..20].copy_from_slice(&gcount_table_off.to_le_bytes());
    h[20..24].copy_from_slice(&0u32.to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    h[28..32].copy_from_slice(&0u32.to_le_bytes());

    // Blocks decompress back-to-back into the payload image. Block sizes are
    // not recorded per block (the image is contiguous); blocks and granules
    // are 1:1, blocks tile the image in granule order, and exact fit is
    // enforced on the final cursor. Per-block image length: LZ4 blocks carry
    // no length, so a dry parse (`block_image_len`) recovers it before the
    // copy; zstd frames bound their own output against the remaining span
    // and report the decoded length.
    let img_len = hdr.uncompressed_len as usize;
    // The dict layers expand frame by frame: block b's image span is
    // frame b's (the frame table is the block geometry by construction).
    let frame_starts = match wrapper {
        Wrapper::FrontZstd => {
            let ft = hdr.frame_table(section)?.unwrap_or_default();
            if ft.len() != nblocks {
                return Err(FormatError::Corrupt { at: "front-coded block geometry" });
            }
            ft
        }
        _ => Vec::new(),
    };
    let mut d = 0usize;
    for b in 0..nblocks {
        if stop() {
            scratch.clear();
            return Ok(false);
        }
        let src = &wrapped_payload[offs[b]..offs[b + 1]];
        let take = match wrapper {
            Wrapper::FrontZstd => {
                let f1 = if b + 1 < nblocks { frame_starts[b + 1] as usize } else { img_len };
                if frame_starts[b] as usize != d || f1 < d || f1 > img_len {
                    return Err(FormatError::Corrupt { at: "front-coded block geometry" });
                }
                let dst = &mut scratch[STREAM_SECTION_HDR_LEN + d..STREAM_SECTION_HDR_LEN + f1];
                unwrap_dict_payload_block(
                    wrapper,
                    src,
                    dst,
                    dict_frame_records(index.expect("adjudicated at entry"), b as u32)?,
                )?;
                f1 - d
            }
            Wrapper::PackedZstd => {
                if nblocks != 1 {
                    return Err(FormatError::Corrupt { at: "packed index block geometry" });
                }
                let dst = &mut scratch[STREAM_SECTION_HDR_LEN..STREAM_SECTION_HDR_LEN + img_len];
                unwrap_block_into(wrapper, src, dst)?;
                img_len
            }
            Wrapper::Lz4 => {
                let take = block_image_len(src)?;
                if d + take > img_len {
                    return Err(FormatError::Corrupt {
                        at: "wrapper image overflow",
                    });
                }
                let dst =
                    &mut scratch[STREAM_SECTION_HDR_LEN + d..STREAM_SECTION_HDR_LEN + d + take];
                lz4::decompress_into(src, dst)?;
                take
            }
            Wrapper::Zstd => {
                let dst =
                    &mut scratch[STREAM_SECTION_HDR_LEN + d..STREAM_SECTION_HDR_LEN + img_len];
                zstd_decompress_block(src, dst)?
            }
            Wrapper::IntPre => {
                let dst =
                    &mut scratch[STREAM_SECTION_HDR_LEN + d..STREAM_SECTION_HDR_LEN + img_len];
                intpre::unwrap_block(src, dst)?
            }
            // Adjudicated at entry; kept typed (never a panic on any input).
            Wrapper::None | Wrapper::FrontOnPairZstd | Wrapper::OnPairZstd => {
                return Err(FormatError::Corrupt {
                    at: "unwrap of unwrapped section",
                })
            }
        };
        d += take;
    }
    if d != img_len {
        return Err(FormatError::Corrupt {
            at: "wrapper image short",
        });
    }
    let tstart = STREAM_SECTION_HDR_LEN + img_len;
    scratch[tstart..].copy_from_slice(tables);
    Ok(true)
}

/// [stack] Decompress ONE §6.4 wrapper block into exactly `dst` — the
/// block-lazy dict-payload serving primitive (RESULTS-FMTLAND §B.3): the
/// caller resolved the block's compressed span from the block-offset table
/// and its uncompressed span from the frame table; a decode that does not
/// fill `dst` exactly is typed corruption. No section header involved.
pub fn unwrap_block_into(wrapper: Wrapper, src: &[u8], dst: &mut [u8]) -> FormatResult<()> {
    let take = match wrapper {
        Wrapper::Lz4 => {
            let take = block_image_len(src)?;
            if take != dst.len() {
                return Err(FormatError::Corrupt {
                    at: "wrapper block image len",
                });
            }
            lz4::decompress_into(src, dst)?;
            take
        }
        Wrapper::Zstd => zstd_decompress_block(src, dst)?,
        Wrapper::IntPre => intpre::unwrap_block(src, dst)?,
        // The packed image is never larger than the 12 B records it packs.
        Wrapper::PackedZstd => {
            let packed = zstd_inflate_bounded(src, dst.len().max(16))?;
            expand_packed_dict_index(&packed, dst)?;
            dst.len()
        }
        Wrapper::FrontZstd | Wrapper::FrontOnPairZstd => {
            return Err(FormatError::Corrupt {
                at: "front-coded dict payload block without the dict index",
            })
        }
        Wrapper::None | Wrapper::OnPairZstd => {
            return Err(FormatError::Corrupt {
                at: "unwrap of unwrapped block",
            })
        }
    };
    if take != dst.len() {
        return Err(FormatError::Corrupt {
            at: "wrapper block image len",
        });
    }
    Ok(())
}

/// The resident front-coded form of a FrontZstd DictPayload section (lane dictresident): the
/// blocks inflated to their front-coded frames and laid out as [`pgrc_format::dict::FcResident`]
/// under an unwrapped twin header with no tables — never expanded. `index` is the v1 index image.
pub fn unwrap_dict_payload_fc_section(section: &[u8], index: &[u8], scratch: &mut Vec<u8>) -> FormatResult<()> {
    unwrap_dict_payload_fc_section_until(section, index, scratch, &mut || false).map(drop)
}

/// D3973: [`unwrap_dict_payload_fc_section`] asking `stop` before each block: `Ok(false)` when it said stop, `scratch` empty.
pub fn unwrap_dict_payload_fc_section_until(section: &[u8], index: &[u8], scratch: &mut Vec<u8>, stop: &mut dyn FnMut() -> bool) -> FormatResult<bool> {
    let hdr = StreamSectionHdr::decode(section)?;
    if Wrapper::from_u8(hdr.wrapper)? == Wrapper::FrontOnPairZstd {
        return crate::onpairwrap::unwrap_dict_payload_fc(&hdr, section, index, scratch, stop);
    }
    if Wrapper::from_u8(hdr.wrapper)? != Wrapper::FrontZstd {
        return Err(FormatError::Corrupt { at: "front-coded resident form of a non-FrontZstd section" });
    }
    dict_payload_len_checked(&hdr, index)?;
    let payload_end = if hdr.frame_table_off != 0 { hdr.frame_table_off as usize } else { section.len() };
    if payload_end < STREAM_SECTION_HDR_LEN + 4 || payload_end > section.len() {
        return Err(FormatError::Bounds { at: "payload region" });
    }
    let wrapped = &section[STREAM_SECTION_HDR_LEN..payload_end];
    let first = u32::from_le_bytes(wrapped[..4].try_into().expect("len 4")) as usize;
    if first < 4 || first % 4 != 0 || first > wrapped.len() {
        return Err(FormatError::Corrupt { at: "wrapper block table" });
    }
    let nblocks = first / 4 - 1;
    let offs: Vec<usize> = (0..=nblocks).map(|i| u32::from_le_bytes(wrapped[i * 4..i * 4 + 4].try_into().expect("len 4")) as usize).collect();
    if offs.windows(2).any(|w| w[0] > w[1]) || *offs.last().expect("nonempty") > wrapped.len() {
        return Err(FormatError::Corrupt { at: "wrapper block offsets" });
    }
    let mut body = Vec::new();
    let mut stopped = false;
    let built = pgrc_format::dict::fc_resident_build(index, &mut |b, out| {
        if stop() {
            stopped = true;
            return Err(FormatError::Corrupt { at: "front-coded resident form stopped" });
        }
        let src = offs.get(b as usize..b as usize + 2).ok_or(FormatError::Corrupt { at: "front-coded block geometry" })?;
        let recs = dict_frame_records(index, b)?;
        let bound = recs.len() / pgrc_format::dict::DICT_INDEX_ENTRY_LEN * 8 + (0..recs.len() / 12).map(|i| u32::from_le_bytes(recs[i * 12 + 4..i * 12 + 8].try_into().expect("len 4")) as usize).sum::<usize>();
        let fc = zstd_inflate_bounded(&wrapped[src[0]..src[1]], bound + 8)?;
        out.extend_from_slice(&fc);
        Ok(())
    }, &mut body);
    scratch.clear();
    if stopped {
        return Ok(false);
    }
    built?;
    scratch.resize(STREAM_SECTION_HDR_LEN, 0);
    let h = &mut scratch[..STREAM_SECTION_HDR_LEN];
    h[0..4].copy_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&hdr.encoding.to_le_bytes());
    h[6] = hdr.width;
    h[7] = Wrapper::None.as_u8();
    h[8..12].copy_from_slice(&hdr.frame_count.to_le_bytes());
    h[24..28].copy_from_slice(&hdr.value_count.to_le_bytes());
    // the frames' copy FILL_PIECE bytes at a time, `stop` asked before each piece past the first
    if scratch.try_reserve_exact(body.len()).is_err() {
        scratch.clear();
        return Err(FormatError::Bounds { at: "front-coded resident form reservation" });
    }
    for (i, piece) in body.chunks(FILL_PIECE).enumerate() {
        if i > 0 && stop() {
            scratch.clear();
            return Ok(false);
        }
        scratch.extend_from_slice(piece);
    }
    Ok(true)
}

/// [stack] The DictPayload twin of [`unwrap_block_into`]: `records` are
/// the v1 index records of the block's dict frame. A `FrontZstd` block
/// inflates to its front-coded frame (never larger than the raw frame)
/// and expands into exactly `dst`; other wrappers ignore `records`.
pub fn unwrap_dict_payload_block(
    wrapper: Wrapper,
    src: &[u8],
    dst: &mut [u8],
    records: &[u8],
) -> FormatResult<()> {
    match wrapper {
        Wrapper::FrontZstd => {
            let fc = zstd_inflate_bounded(src, dst.len() + 8)?;
            expand_front_coded_frame(&fc, records, dst)
        }
        _ => unwrap_block_into(wrapper, src, dst),
    }
}

/// Exact decoded length of one LZ4 block (a dry parse: lengths only, no
/// byte copies). Bounds-validated; typed errors.
fn block_image_len(src: &[u8]) -> FormatResult<usize> {
    let mut s = 0usize;
    let mut d = 0usize;
    loop {
        let token = *src
            .get(s)
            .ok_or(FormatError::Truncated { at: "lz4 token" })?;
        s += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src
                    .get(s)
                    .ok_or(FormatError::Truncated { at: "lz4 litlen" })?;
                s += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
                if lit > isize::MAX as usize / 2 {
                    return Err(FormatError::Corrupt { at: "lz4 litlen" });
                }
            }
        }
        if s + lit > src.len() {
            return Err(FormatError::Truncated { at: "lz4 literals" });
        }
        s += lit;
        d += lit;
        if s == src.len() {
            return Ok(d);
        }
        s += 2; // offset
        if s > src.len() {
            return Err(FormatError::Truncated { at: "lz4 offset" });
        }
        let mut mlen = (token & 0x0F) as usize + 4;
        if mlen == 19 {
            loop {
                let b = *src
                    .get(s)
                    .ok_or(FormatError::Truncated { at: "lz4 matlen" })?;
                s += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
                if mlen > isize::MAX as usize / 2 {
                    return Err(FormatError::Corrupt { at: "lz4 matlen" });
                }
            }
        }
        d += mlen;
    }
}

// ---------------------------------------------------------------------------
// CMP-F meta-plane envelope (SB-6)
// ---------------------------------------------------------------------------

/// Wrap a META-section body (Stats/Psma/Bloom/NdvRegisters — the CMP-F
/// plane) in the `[raw_len: u32 LE][zstd frame]` envelope. Returns `None`
/// when zstd is unavailable in this build (the wasm parity-honest
/// degradation: the section ships raw, exactly like the value path).
/// The SB-2 ≥20% gate is the CALLER's — this function only builds the
/// candidate envelope.
pub fn meta_wrap_body(raw: &[u8]) -> Option<Vec<u8>> {
    if !wrapper_available(Wrapper::Zstd) || raw.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(4 + raw.len() / 2);
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    zstd_compress_block(raw, &mut out).ok()?;
    Some(out)
}

/// Unwrap a `SECTIONF_META_ZSTD` meta-section envelope back to the raw
/// body. The stored bytes' CRC was already validated by the section reader
/// (spec §5.2); this validates the envelope's own length claim.
pub fn meta_unwrap_body(envelope: &[u8]) -> FormatResult<Vec<u8>> {
    if envelope.len() < 4 {
        return Err(FormatError::Truncated {
            at: "meta envelope raw_len",
        });
    }
    let raw_len = u32::from_le_bytes(envelope[..4].try_into().expect("len 4")) as usize;
    let mut out = vec![0u8; raw_len];
    let n = zstd_decompress_block(&envelope[4..], &mut out)
        .map_err(|_| FormatError::Corrupt {
            at: "meta envelope zstd frame",
        })?;
    if n != raw_len {
        return Err(FormatError::Corrupt {
            at: "meta envelope raw_len claim",
        });
    }
    Ok(out)
}
