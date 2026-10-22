//! Wave 3 A2 (cvformat): a section written through `StreamSectionWriter::begin_charged` / `OverflowSink::new_charged`
//! into the caller's `ChargedVec`s holds exactly the bytes of the plain path, its blocks are charged to the vecs' owner
//! after the writer is done (capacity, the writer's side tables given back), and nothing stays after the vecs drop.

use crate::abi::{EncodeInput, GranuleEncoder};
use crate::bankstats::{build_image, build_image_for, ColInput, PartIdent};
use crate::manifest::{Manifest, ManifestHeader, MANIFEST_MAGIC};
use crate::sections::{assemble, assemble_for, SECTION_ANSWERS, SECTION_FACTS};
use crate::class::StorageClass;
use crate::enc::Wrapper;
use crate::geom::OVERSIZE_THRESHOLD;
use crate::part::{OverflowSink, StreamSectionHdr, StreamSectionWriter};
use crate::sidecar::{encode_stats_payload, encode_stats_payload_for, ColDistribution};
use crate::verbatim::VerbatimEncoder;
use crate::wire::varlena_header_4b_u;
use memledger::charge::{context, owner_row};
use memledger::{bind_backend, statement_begin, statement_end, unbind_backend, ChargedVec, Kind};

fn images(g: usize) -> Vec<Vec<u8>> {
    (0..3000usize)
        .map(|r| {
            let mut s = format!("granule-{g}-row-{r}-").into_bytes();
            s.resize(if r == 77 { OVERSIZE_THRESHOLD as usize + 99 } else { 60 + r % 50 }, b'q');
            let mut img = varlena_header_4b_u(s.len() as u32).to_le_bytes().to_vec();
            img.extend_from_slice(&s);
            img
        })
        .collect()
}

fn encode(inputs: &[EncodeInput<'_>], sec: &mut StreamSectionWriter<'_>, ovf: &mut OverflowSink<'_>) {
    let mut enc = VerbatimEncoder { class: StorageClass::VarlenaVerbatim };
    for g in inputs {
        enc.encode_granule(g, sec, ovf).expect("encodes");
    }
    enc.finish_stream(sec).expect("finishes");
}

#[test]
fn a_charged_section_holds_the_plain_bytes_on_its_owner() {
    std::thread::spawn(|| {
        bind_backend(3972, 8172);
        statement_begin();
        let (o, _) = context();
        let imgs: Vec<Vec<Vec<u8>>> = (0..2).map(images).collect();
        let datums: Vec<Vec<u64>> = imgs.iter().map(|g| g.iter().map(|i| i.as_ptr() as u64).collect()).collect();
        let inputs: Vec<EncodeInput<'_>> = datums
            .iter()
            .map(|d| EncodeInput { class: StorageClass::VarlenaVerbatim, rows: d.len() as u32, datums: d, validity: None })
            .collect();
        let key = VerbatimEncoder { class: StorageClass::VarlenaVerbatim }.key();
        let (mut psec, mut povf) = (Vec::new(), Vec::new());
        {
            let mut w = StreamSectionWriter::begin(&mut psec, key.encoding, key.width, Wrapper::None).unwrap();
            let mut ovf = OverflowSink::new(&mut povf);
            assert_eq!((w.owner(), ovf.owner()), (None, None));
            encode(&inputs, &mut w, &mut ovf);
            w.finish(true).unwrap();
        }
        let mut sec: ChargedVec<u8> = ChargedVec::new(Some(o), Kind::Region);
        let mut ov: ChargedVec<u8> = ChargedVec::new(Some(o), Kind::Region);
        {
            let mut w = StreamSectionWriter::begin_charged(&mut sec, key.encoding, key.width, Wrapper::None).unwrap();
            let mut ovf = OverflowSink::new_charged(&mut ov);
            assert_eq!((w.owner(), ovf.owner()), (Some(o), Some(o)));
            encode(&inputs, &mut w, &mut ovf);
            assert!(owner_row(o).committed > 0, "charged while the writer works");
            w.finish(true).unwrap();
        }
        assert_eq!((sec.as_slice(), ov.as_slice()), (psec.as_slice(), povf.as_slice()), "the same bytes");
        assert!(!ov.is_empty(), "the oversize value went to the overflow region");
        assert_eq!(owner_row(o).committed, (sec.capacity() + ov.capacity()) as i64, "the blocks' capacity, side tables back");
        // A4: the frame table and a stats payload in ChargedVecs on the same owner: the plain forms' values.
        let hdr = StreamSectionHdr::decode(&sec).unwrap();
        let plain = hdr.frame_table(&sec).unwrap().expect("framed");
        let ft = hdr.frame_table_for(&sec, Some((o, Kind::Region))).unwrap().expect("framed");
        assert_eq!(ft.as_slice(), plain.as_slice());
        assert_eq!(hdr.frame_table_for(&sec, None).unwrap().expect("framed").charged_bytes(), 0);
        let d = ColDistribution { nonnull: 9, ndv_eligible: 2, long_values: 1, mcv: vec![(b"ab".to_vec(), 5)], hist_bounds: vec![b"a".to_vec(), b"zz".to_vec()] };
        let cols = [(1u32, 0u32, d.clone()), (3, 1, d)];
        let st = encode_stats_payload_for(&cols, Some((o, Kind::Region)));
        assert_eq!(st.as_slice(), encode_stats_payload(&cols).as_slice());
        let held = (sec.capacity() + ov.capacity() + 4 * ft.capacity() + st.capacity()) as i64;
        assert_eq!(owner_row(o).committed, held);
        // A4: the bank plane image and its column payloads on the same owner: the plain image's bytes, the image's
        // capacity held after the build (the payloads given back).
        let parts: Vec<PartIdent> = (0..40u32)
            .map(|i| PartIdent { part_no: i, granule_count: 3, band_count: 1, rows: 1000 + u64::from(i), footer_off: 4096 })
            .collect();
        let bodies: Vec<Vec<u8>> = (0..40usize).map(|i| vec![i as u8; 64 + i]).collect();
        let pcols: Vec<ColInput<'_>> = (1..=5u32)
            .map(|a| ColInput {
                attno: a,
                stats_bodies: bodies.iter().map(Vec::as_slice).collect(),
                digests: (0..40usize).map(|i| (i % 3 == 0).then_some([a as u8; crate::bankstats::BANKSTATS_DIGEST_LEN])).collect(),
            })
            .collect();
        let before = owner_row(o).committed;
        let img = build_image_for(7, 99, &parts, &pcols, Some((o, Kind::Region))).unwrap();
        assert_eq!(img.as_slice(), build_image(7, 99, &parts, &pcols).unwrap().as_slice());
        assert_eq!(owner_row(o).committed - before, img.capacity() as i64);
        assert_eq!(build_image_for(7, 99, &parts, &pcols, None).unwrap().charged_bytes(), 0);
        // A4: a catalog sections file on the same owner: reserved whole, so its capacity is its length.
        let m = Manifest {
            header: ManifestHeader { gen: 3, prev_gen: 2, publisher_fxid: 9, relfilenumber: 7, schema_fingerprint: 1, magic: MANIFEST_MAGIC, format_version: crate::FORMAT_VERSION, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 },
            parts: Vec::new(),
            ext: Vec::new(),
        };
        let (facts, col) = (b"facts bytes".to_vec(), vec![7u8; 13]);
        let secs: [(u32, u32, &[u8]); 2] = [(SECTION_FACTS, 0, &facts), (SECTION_ANSWERS, 4, &col)];
        let before = owner_row(o).committed;
        let sf = assemble_for(&m, 77, &secs, Some((o, Kind::Region))).unwrap();
        assert_eq!(sf.as_slice(), assemble(&m, 77, &secs).unwrap().as_slice());
        assert_eq!(owner_row(o).committed - before, sf.capacity() as i64);
        drop((sec, ov, ft, st, img, sf));
        assert_eq!(owner_row(o).committed, 0);
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}
