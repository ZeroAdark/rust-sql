//! D4409 (b): a stream's bands are each kept in the form they win in alone (the wrapped image when it clears the 20%
//! law on its own, else the unwrapped section), and the stream-level 20% decision over every band's totals then puts
//! each band in the stream's form: a band kept unwrapped in a stream that wraps is wrapped again, a band kept wrapped
//! in a stream that does not wrap is unwrapped back. Both directions seal the bytes the seal wrote when it held every
//! band's unwrapped section to the stream's end: the part file and its stats payload are pinned to that seal's.

#![cfg(not(loom))]

use pgrc_format::abi::ColumnMetaBuilder;
use pgrc_format::enc::Wrapper;
use pgrc_format::part::StreamRole;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture};
use pgrc_write::ingest::ColBuffer;
use pgrc_write::seal::{seal_part, CodecResolver, PartSpec};
use pgrc_write::structural::StructuralPolicy;
use pgrc_write::testkit::{mem_with_dir, text_col, PartView, DB, DIR, RELFILENUMBER, SPC};

const BAND_ROWS: u64 = pgrc_format::geom::BAND_ROWS as u64;

/// splitmix64's finalizer.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One text column, band by band: a packing band holds `'p' x 110 || i` (its wrap wins alone), a random band 128
/// pseudo-random bytes per value (its wrap loses alone).
fn column(packs: &[bool]) -> ColBuffer {
    let mut c = ColBuffer::new(text_col(1));
    for (b, &p) in packs.iter().enumerate() {
        for i in b as u64 * BAND_ROWS..(b as u64 + 1) * BAND_ROWS {
            if p {
                c.append_varlena_payload(format!("{}{i}", "p".repeat(110)).as_bytes()).expect("text");
            } else {
                let mut v = Vec::with_capacity(128);
                for j in 0..16 {
                    v.extend(mix(i * 16 + j).to_le_bytes().map(|x| x.max(1)));
                }
                c.append_varlena_payload(&v).expect("text");
            }
        }
    }
    c
}

/// Seals `packs`' column as a COPY does (CodecCandidates + CodecResolver); -> (the Values stream's wrapper, the part
/// file's length and CRC-32C, the stats payload's CRC-32C).
fn seal(packs: &[bool]) -> (u8, u64, u32, u32) {
    let roots = vec![column(packs)];
    let mut builders: Vec<Box<dyn ColumnMetaBuilder>> = pgrc_write::meta_wire::builders_for_streams(&roots, &[], &[]);
    let mut vfs = mem_with_dir();
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0x4409 };
    let cands = CodecCandidates::new(ColumnPosture { fused: false, cold: true, numeric: false, dict: None });
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let (sealed, _) = seal_part(&mut vfs, DIR, &spec, &roots, &[], &[], &mut builders, &sources, &CodecResolver, &StructuralPolicy::default(), 4409, 0)
        .expect("seal");
    let view = PartView::open(&mut vfs, &sealed.tmp_name);
    let (entry, extents) = view.stream(1, 0, StreamRole::Values).expect("values stream");
    assert_eq!(extents.len(), packs.len(), "one extent per band");
    let file_crc = pgrc_format::wire::crc32c(&view.bytes);
    let stats_crc = pgrc_format::wire::crc32c(&sealed.stats_payload);
    eprintln!("d4409: wrapper {} file {} B crc {file_crc:#010x} stats crc {stats_crc:#010x}", entry.wrapper, view.bytes.len());
    (entry.wrapper, view.bytes.len() as u64, file_crc, stats_crc)
}

/// Three packing bands and one random band: the stream wraps, and the random band, kept unwrapped, is wrapped again.
#[ignore = "V5SYNC r14 FOLLOWUP: D4409(b) band forms not wired into the line's streamed seal (PGRCSERVE M4 / post-switch)"]
#[test]
fn a_stream_that_wraps_rewraps_the_band_that_lost_alone() {
    let (wrapper, len, crc, stats) = seal(&[true, true, false, true]);
    assert_eq!(wrapper, Wrapper::Zstd.as_u8(), "the stream wraps");
    assert_eq!((len, crc, stats), WRAPS_PIN, "the bytes of the seal that held every band to the stream's end");
}

/// One packing band among six random ones: the stream does not wrap, and the packing band, kept wrapped, is unwrapped
/// back.
#[ignore = "V5SYNC r14 FOLLOWUP: D4409(b) band forms not wired into the line's streamed seal (PGRCSERVE M4 / post-switch)"]
#[test]
fn a_stream_that_does_not_wrap_unwraps_the_band_that_won_alone() {
    let (wrapper, len, crc, stats) = seal(&[false, false, false, true, false, false, false]);
    assert_eq!(wrapper, Wrapper::None.as_u8(), "the stream does not wrap");
    assert_eq!((len, crc, stats), PLAIN_PIN, "the bytes of the seal that held every band to the stream's end");
}

/// (file length, file CRC-32C, stats payload CRC-32C) of trunk's seal (every band held to the stream's end).
const WRAPS_PIN: (u64, u32, u32) = (8_890_520, 0x5e49_0072, 0x9608_6183);
const PLAIN_PIN: (u64, u32, u32) = (63_721_784, 0xd9b1_a0c7, 0x9372_354a);
