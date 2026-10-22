//! Section-grain round-trips of the pgrc format 2.2 dictionary wrappers (spec
//! §6.4/§7): `FrontZstd` over a framed DictPayload section and
//! `PackedZstd` over a DictIndex section rebuild the writer's unwrapped
//! section images byte-for-byte through both the whole-section and the
//! block-grain unwrap faces; the role-agnostic faces refuse the
//! index-needing arm typed.

use crate::wrapper::{
    unwrap_block_into, unwrap_dict_payload_block, unwrap_dict_payload_fc_section, unwrap_dict_payload_fc_section_until, unwrap_dict_payload_section,
    unwrap_dict_payload_section_until, unwrap_section, unwrap_section_until,
    wrap_dict_index_section, wrap_dict_payload_section, wrap_section, FILL_PIECE,
};
use pgrc_format::dict::{dict_frame_records, utf8_char_count, DictIndexEntry};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::geom::DICT_FRAME_ENTRIES;
use pgrc_format::part::{StreamSectionHdr, StreamSectionWriter, STREAM_SECTION_HDR_LEN};
use pgrc_format::wire::put_varlena_entry;
use pgrc_format::FormatError;

/// The writer's two dict sections for a sorted, distinct entry list
/// (`pgrc_write::dict::emit_sections` verbatim).
pub(super) fn dict_sections(entries: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let enc = EncodingId::DictCodes.as_u16();
    let mut payload = Vec::new();
    let mut offs = Vec::new();
    {
        let mut w = StreamSectionWriter::begin(&mut payload, enc, 0, Wrapper::None).expect("begin");
        for (code, e) in entries.iter().enumerate() {
            if code as u32 % DICT_FRAME_ENTRIES == 0 {
                w.align_payload(8);
                w.begin_frame();
            }
            w.align_payload(8);
            let off = w.payload_off();
            let base = w.payload().len() - off as usize;
            let abs = put_varlena_entry(w.payload(), e);
            offs.push((abs as usize - base) as u32);
        }
        w.end_granule(entries.len() as u32);
        w.finish(false).expect("finish");
    }
    let mut index = Vec::new();
    {
        let mut w = StreamSectionWriter::begin(&mut index, enc, 0, Wrapper::None).expect("begin");
        for (code, e) in entries.iter().enumerate() {
            DictIndexEntry { payload_off: offs[code], byte_len: e.len() as u32, char_field: utf8_char_count(e) }
                .encode_into(w.payload());
        }
        w.end_granule(entries.len() as u32);
        w.finish(false).expect("finish");
    }
    (index, payload)
}

fn corpus(n: usize) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("https://example.org/catalog/section-{:03}/item-{i:07}?ref=Москва", i % 7).into_bytes())
        .collect();
    v.push(Vec::new());
    v.sort();
    v.dedup();
    v
}

pub(super) fn index_payload(index_section: &[u8]) -> &[u8] {
    &index_section[STREAM_SECTION_HDR_LEN..]
}

#[test]
fn dict_wrappers_round_trip_whole_and_block_grain() {
    for n in [1usize, 33, 1024, 2500] {
        let entries = corpus(n);
        let (index, payload) = dict_sections(&entries);
        let mut w_index = Vec::new();
        wrap_dict_index_section(&index, &mut w_index).expect("wrap index");
        let mut w_payload = Vec::new();
        wrap_dict_payload_section(&payload, index_payload(&index), &mut w_payload).expect("wrap payload");
        assert_eq!(StreamSectionHdr::decode(&w_index).expect("hdr").wrapper, Wrapper::PackedZstd.as_u8());
        assert_eq!(StreamSectionHdr::decode(&w_payload).expect("hdr").wrapper, Wrapper::FrontZstd.as_u8());

        // Whole-section unwraps rebuild the writer's images exactly.
        let mut scratch = Vec::new();
        unwrap_section(&w_index, &mut scratch).expect("unwrap index");
        assert!(scratch == index, "index image ({n} entries)");
        unwrap_dict_payload_section(&w_payload, index_payload(&index), &mut scratch).expect("unwrap payload");
        assert!(scratch == payload, "payload image ({n} entries)");

        // Block grain: every frame block expands into its raw span.
        let hdr = StreamSectionHdr::decode(&w_payload).expect("hdr");
        let ft = hdr.frame_table(&w_payload).expect("ft").expect("framed");
        let wrapped = &w_payload[STREAM_SECTION_HDR_LEN..hdr.frame_table_off as usize];
        let nb = ft.len();
        let btab: Vec<usize> = (0..=nb)
            .map(|i| u32::from_le_bytes(wrapped[i * 4..i * 4 + 4].try_into().expect("len 4")) as usize)
            .collect();
        let raw_payload = &payload[STREAM_SECTION_HDR_LEN..];
        let img_len = hdr.uncompressed_len as usize;
        assert_eq!(img_len, raw_payload.len() - (payload.len() - STREAM_SECTION_HDR_LEN - img_len));
        let mut img = vec![0u8; img_len];
        for b in 0..nb {
            let (f0, f1) = (ft[b] as usize, if b + 1 < nb { ft[b + 1] as usize } else { img_len });
            let recs = dict_frame_records(index_payload(&index), b as u32).expect("records");
            unwrap_dict_payload_block(Wrapper::FrontZstd, &wrapped[btab[b]..btab[b + 1]], &mut img[f0..f1], recs)
                .expect("block");
        }
        assert!(img[..] == raw_payload[..img_len], "block-grain image ({n} entries)");
        // The one-block index also serves through the block face.
        let ihdr = StreamSectionHdr::decode(&w_index).expect("hdr");
        let iw = &w_index[STREAM_SECTION_HDR_LEN..];
        let end = u32::from_le_bytes(iw[4..8].try_into().expect("len 4")) as usize;
        let mut irec = vec![0u8; ihdr.uncompressed_len as usize];
        unwrap_block_into(Wrapper::PackedZstd, &iw[8..end], &mut irec).expect("index block");
        assert!(irec[..] == index[STREAM_SECTION_HDR_LEN..]);
    }
}

/// D3887: the whole-section unwrap asks its stop before every block: a stop mid-way ends it with nothing built, and a stop
/// that never says stop rebuilds the same image as the plain face.
#[test]
fn dict_payload_unwrap_stops_between_blocks_when_asked() {
    let entries = corpus(2500);
    let (index, payload) = dict_sections(&entries);
    let mut w_payload = Vec::new();
    wrap_dict_payload_section(&payload, index_payload(&index), &mut w_payload).expect("wrap payload");
    let hdr = StreamSectionHdr::decode(&w_payload).expect("hdr");
    let nblocks = hdr.frame_table(&w_payload).expect("ft").expect("framed").len();
    assert!(nblocks >= 3, "{nblocks} blocks");
    let (mut scratch, mut asked) = (Vec::new(), 0usize);
    let done = unwrap_dict_payload_section_until(&w_payload, index_payload(&index), &mut scratch, &mut || {
        asked += 1;
        false
    })
    .expect("unwrap");
    assert!(done && scratch == payload, "the full image");
    assert_eq!(asked, nblocks, "asked once per block");
    asked = 0;
    let done = unwrap_dict_payload_section_until(&w_payload, index_payload(&index), &mut scratch, &mut || {
        asked += 1;
        asked == 2
    })
    .expect("unwrap");
    assert!(!done && scratch.is_empty(), "stopped before the second block, nothing kept");
}

/// D3973: the resident front-coded form asks its stop before every block, as the expanded one does (D3887).
#[test]
fn dict_payload_fc_unwrap_stops_between_blocks_when_asked() {
    let entries = corpus(2500);
    let (index, payload) = dict_sections(&entries);
    let mut w_payload = Vec::new();
    wrap_dict_payload_section(&payload, index_payload(&index), &mut w_payload).expect("wrap payload");
    let nblocks = StreamSectionHdr::decode(&w_payload).expect("hdr").frame_table(&w_payload).expect("ft").expect("framed").len();
    assert!(nblocks >= 3, "{nblocks} blocks");
    let mut whole = Vec::new();
    unwrap_dict_payload_fc_section(&w_payload, index_payload(&index), &mut whole).expect("fc unwrap");
    let (mut scratch, mut asked) = (Vec::new(), 0usize);
    let done = unwrap_dict_payload_fc_section_until(&w_payload, index_payload(&index), &mut scratch, &mut || {
        asked += 1;
        false
    })
    .expect("fc unwrap");
    assert!(done && scratch == whole, "the full resident form");
    assert_eq!(asked, nblocks, "asked once per block");
    asked = 0;
    let done = unwrap_dict_payload_fc_section_until(&w_payload, index_payload(&index), &mut scratch, &mut || {
        asked += 1;
        asked == 2
    })
    .expect("fc unwrap");
    assert!(!done && scratch.is_empty(), "stopped before the second block, nothing kept");
}

/// D4228: a large block's image is zero-filled and faulted in a FILL_PIECE at a time, the stop asked between pieces: one
/// 4 MiB block used to fill its whole image before the first ask (the block loop's), and a stop mid-fill keeps nothing.
#[test]
fn a_one_block_image_asks_its_stop_between_fill_pieces() {
    let mut section = Vec::new();
    {
        let mut w = StreamSectionWriter::begin(&mut section, EncodingId::DictCodes.as_u16(), 0, Wrapper::None).expect("begin");
        w.payload().extend((0..4 * FILL_PIECE).map(|i| (i % 251) as u8));
        w.end_granule(1);
        w.finish(false).expect("finish");
    }
    let mut z = Vec::new();
    wrap_section(&section, &[4 * FILL_PIECE as u32], Wrapper::Zstd, &mut z).expect("zstd");
    let (mut scratch, mut asked) = (Vec::new(), 0usize);
    let done = unwrap_section_until(&z, &mut scratch, &mut || {
        asked += 1;
        false
    })
    .expect("unwrap");
    assert!(done && scratch == section, "the full image");
    assert!(asked > 4, "asked {asked} times for one 4 MiB block: once per fill piece and once for the block");
    asked = 0;
    let done = unwrap_section_until(&z, &mut scratch, &mut || {
        asked += 1;
        asked == 2
    })
    .expect("unwrap");
    assert!(!done && scratch.is_empty(), "stopped mid-fill, nothing kept");
}

#[test]
fn front_coded_payload_refuses_the_role_agnostic_faces_typed() {
    let entries = corpus(200);
    let (index, payload) = dict_sections(&entries);
    let mut w_payload = Vec::new();
    wrap_dict_payload_section(&payload, index_payload(&index), &mut w_payload).expect("wrap");
    let mut scratch = Vec::new();
    assert!(matches!(unwrap_section(&w_payload, &mut scratch), Err(FormatError::Corrupt { .. })));
    let mut dst = vec![0u8; 64];
    assert!(matches!(unwrap_block_into(Wrapper::FrontZstd, &[0u8; 4], &mut dst), Err(FormatError::Corrupt { .. })));
    // The dict arms are not `wrap_section` arms (role-bound by design).
    assert!(matches!(
        wrap_section(&payload, &[payload.len() as u32 - STREAM_SECTION_HDR_LEN as u32], Wrapper::FrontZstd, &mut Vec::new()),
        Err(FormatError::EncodeContract { .. })
    ));
}

#[test]
fn dict_wrappers_beat_plain_zstd_on_prefix_heavy_entries() {
    let entries = corpus(3000);
    let (index, payload) = dict_sections(&entries);
    let hdr = StreamSectionHdr::decode(&payload).expect("hdr");
    let ends: Vec<u32> = {
        let ft = hdr.frame_table(&payload).expect("ft").expect("framed");
        ft.iter().skip(1).copied().chain(core::iter::once(hdr.frame_table_off - STREAM_SECTION_HDR_LEN as u32)).collect()
    };
    let mut z = Vec::new();
    wrap_section(&payload, &ends, Wrapper::Zstd, &mut z).expect("zstd");
    let mut fz = Vec::new();
    wrap_dict_payload_section(&payload, index_payload(&index), &mut fz).expect("front zstd");
    assert!(fz.len() < z.len(), "front-coded beneath zstd {} vs zstd {}", fz.len(), z.len());
    let mut zi = Vec::new();
    wrap_section(&index, &[index.len() as u32 - STREAM_SECTION_HDR_LEN as u32], Wrapper::Zstd, &mut zi).expect("zstd");
    let mut pi = Vec::new();
    wrap_dict_index_section(&index, &mut pi).expect("packed");
    assert!(pi.len() < zi.len(), "packed index beneath zstd {} vs zstd {}", pi.len(), zi.len());
}
