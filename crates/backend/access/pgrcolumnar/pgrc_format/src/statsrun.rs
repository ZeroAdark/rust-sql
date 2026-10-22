//! A stats run, `srun-<gen>-<seq>.pgrcr` (P-1, "stats runs"): the planner facts and the part-grain answers of a SET
//! of parts, immutable once written. A generation's stats are the runs its root lists plus the root carrier itself,
//! and at most a few *loose* parts whose stats are read from their own footers; the manifest names the root carrier by
//! its `EXT_STATS` extension record (`manifest::stats_ref`). A publish writes a run only when it folds loose parts,
//! merges runs, or cuts a run a retire touched, so its stats bytes are bounded by a constant, not by the part count.
//!
//! The file is the sections container (`crate::sections`) under magic `PRCR`, version 1; the header's bound crc is
//! `crc32c(schema_fingerprint LE)`. Sections:
//!
//! ```text
//! FACTS   (1, attno 0)   the run's FactsImage over its parts only (zones in the run's part_no order)
//! ANSWERS (2, attno)     × C: answers::encode_column over the run's parts
//! PARTS   (3, attno 0)   bankstats::PartIdent (32 B) × nparts, part_no strictly ascending
//! ROOT    (4, attno 0)   RunRef (32 B) × k, (gen, seq) strictly ascending, each below this run's own (gen, seq);
//!                        only in the run the manifest points at, and never naming itself
//! PARTEXT (5, attno 0)   (part_no u32, tag u16, pad u16 = 0, len u32, bytes)…, (part_no, tag) strictly ascending,
//!                        each part_no one of PARTS (reserved for K1's births)
//! ```
//!
//! A reader holds a `RunRef` (from the manifest or a root) before it reads a run: `RunFile::parse` checks the file's
//! length and whole-file crc against it, so a stale file under a reused name is refused, never misread.

use crate::bankstats::{PartIdent, BANKSTATS_PARTIDENT_LEN};
use crate::facts::FactsImage;
use crate::sections::{assemble_bound, SectionsFile, SECTION_ANSWERS, SECTION_FACTS};
use crate::wire::{crc32c, put_u16, put_u32, put_u64, Cur};
use crate::{FormatError, FormatResult};

pub const RUN_MAGIC: u32 = u32::from_le_bytes(*b"PRCR");
pub const RUN_VERSION: u32 = 1;
/// `bankstats::PartIdent` × n, part_no ascending.
pub const SECTION_RUN_PARTS: u32 = 3;
/// `RunRef` × k (the root carrier only).
pub const SECTION_RUN_ROOT: u32 = 4;
/// `(part_no u32, tag u16, pad u16, len u32, bytes)…` (K1's births).
pub const SECTION_RUN_PARTEXT: u32 = 5;
pub const RUN_REF_LEN: usize = 32;
const PARTEXT_HEAD_LEN: usize = 12;

/// A reference to one run: its writer generation, its sequence in that publish, its part count, its file length and
/// its whole-file crc32c. 32 B on the wire: gen u64, seq u32, nparts u32, len u64, crc u32, pad u32 = 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRef {
    pub gen: u64,
    pub seq: u32,
    pub nparts: u32,
    pub len: u64,
    pub crc: u32,
}

impl RunRef {
    /// The reference to `bytes`, an encoded run written as `(gen, seq)` over `nparts` parts.
    pub fn of(gen: u64, seq: u32, nparts: u32, bytes: &[u8]) -> RunRef {
        RunRef { gen, seq, nparts, len: bytes.len() as u64, crc: crc32c(bytes) }
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u64(out, self.gen);
        put_u32(out, self.seq);
        put_u32(out, self.nparts);
        put_u64(out, self.len);
        put_u32(out, self.crc);
        put_u32(out, 0);
    }

    pub fn decode_from(c: &mut Cur<'_>) -> FormatResult<RunRef> {
        let at = "RunRef";
        let r = RunRef { gen: c.u64(at)?, seq: c.u32(at)?, nparts: c.u32(at)?, len: c.u64(at)?, crc: c.u32(at)? };
        if c.u32(at)? != 0 {
            return Err(FormatError::Corrupt { at: "RunRef pad" });
        }
        Ok(r)
    }

    /// The (gen, seq) order key: a run's identity and its file name.
    pub fn key(&self) -> (u64, u32) {
        (self.gen, self.seq)
    }
}

/// `RunRef` × n.
pub fn try_encode_refs(refs: &[RunRef]) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(refs.len() * RUN_REF_LEN, "RunRef list")?;
    for r in refs {
        r.encode_into(&mut out);
    }
    Ok(out)
}

#[cfg(test)]
pub fn encode_refs(refs: &[RunRef]) -> Vec<u8> {
    try_encode_refs(refs).expect("encode_refs")
}

/// `RunRef` × n; a length that is not a multiple of 32 or a nonzero pad is `Corrupt`.
pub fn decode_refs(b: &[u8]) -> FormatResult<Vec<RunRef>> {
    if !b.len().is_multiple_of(RUN_REF_LEN) {
        return Err(FormatError::Corrupt { at: "RunRef list length" });
    }
    let mut c = Cur::new(b);
    (0..b.len() / RUN_REF_LEN).map(|_| RunRef::decode_from(&mut c)).collect()
}

/// What `encode_run` writes. `answers` are `(attno, answers::encode_column payload)`, attno ascending; `partext` are
/// `(part_no, tag, bytes)`, (part_no, tag) ascending.
pub struct RunImage<'a> {
    pub gen: u64,
    pub publisher_fxid: u64,
    pub schema_fingerprint: u64,
    pub parts: &'a [PartIdent],
    pub facts: &'a FactsImage,
    pub answers: &'a [(u32, &'a [u8])],
    pub root: Option<&'a [RunRef]>,
    pub partext: &'a [(u32, u16, &'a [u8])],
}

fn schema_crc(fp: u64) -> u32 {
    crc32c(&fp.to_le_bytes())
}

/// Encode a run file. The caller keeps the invariants `RunFile::parse` checks; they are debug-asserted here.
pub fn encode_run(img: &RunImage<'_>) -> FormatResult<Vec<u8>> {
    debug_assert!(img.parts.windows(2).all(|w| w[0].part_no < w[1].part_no), "run parts must be part_no ascending");
    debug_assert!(img.answers.windows(2).all(|w| w[0].0 < w[1].0), "run answers must be attno ascending");
    let mut parts = crate::try_vec(img.parts.len() * BANKSTATS_PARTIDENT_LEN, "stats run parts")?;
    for p in img.parts {
        p.encode_into(&mut parts);
    }
    let facts = img.facts.encode()?;
    let root = img.root.map(try_encode_refs).transpose()?;
    let mut partext = Vec::new();
    for (part_no, tag, bytes) in img.partext {
        put_u32(&mut partext, *part_no);
        put_u16(&mut partext, *tag);
        put_u16(&mut partext, 0);
        put_u32(&mut partext, bytes.len() as u32);
        partext.extend_from_slice(bytes);
    }
    let mut sections: Vec<(u32, u32, &[u8])> = crate::try_vec(img.answers.len() + 4, "stats run sections")?;
    sections.push((SECTION_RUN_PARTS, 0, &parts));
    sections.push((SECTION_FACTS, 0, &facts));
    for (attno, payload) in img.answers {
        sections.push((SECTION_ANSWERS, *attno, payload));
    }
    if let Some(root) = &root {
        sections.push((SECTION_RUN_ROOT, 0, root));
    }
    if !img.partext.is_empty() {
        sections.push((SECTION_RUN_PARTEXT, 0, &partext));
    }
    assemble_bound(RUN_MAGIC, RUN_VERSION, img.gen, img.publisher_fxid, schema_crc(img.schema_fingerprint), &sections)
}

/// A parsed run: its identity, its parts and its root list, validated against the `RunRef` it was read by; the facts,
/// answers and part extensions are decoded on demand.
pub struct RunFile {
    pub gen: u64,
    pub publisher_fxid: u64,
    pub parts: Vec<PartIdent>,
    pub root: Option<Vec<RunRef>>,
    file: SectionsFile,
}

impl RunFile {
    /// Parse `bytes`, read as the run `want` names. Refuses: a length, crc, generation or part count other than
    /// `want`'s; a section kind outside the run's set, or FACTS / PARTS / ROOT / PARTEXT other than once (ROOT and
    /// PARTEXT: at most once), or two ANSWERS of one attno; PARTS not strictly ascending by part_no; a ROOT not strictly
    /// ascending by (gen, seq), naming a run at or past `want`'s own (gen, seq), or a run of no parts.
    pub fn parse(bytes: Vec<u8>, want: &RunRef) -> FormatResult<RunFile> {
        if bytes.len() as u64 != want.len {
            return Err(FormatError::Corrupt { at: "stats run length vs ref" });
        }
        if crc32c(&bytes) != want.crc {
            return Err(FormatError::CrcMismatch { at: "stats run" });
        }
        let file = SectionsFile::parse_with(bytes, RUN_MAGIC, RUN_VERSION)?;
        if file.gen != want.gen {
            return Err(FormatError::Corrupt { at: "stats run gen vs ref" });
        }
        let (mut nfacts, mut nparts_sec, mut nroot, mut npartext) = (0, 0, 0, 0);
        let mut attnos: Vec<u32> = Vec::new();
        for e in &file.entries {
            match e.kind {
                SECTION_FACTS => nfacts += 1,
                SECTION_RUN_PARTS => nparts_sec += 1,
                SECTION_RUN_ROOT => nroot += 1,
                SECTION_RUN_PARTEXT => npartext += 1,
                SECTION_ANSWERS => attnos.push(e.attno),
                _ => return Err(FormatError::Corrupt { at: "stats run section kind" }),
            }
            if e.kind != SECTION_ANSWERS && e.attno != 0 {
                return Err(FormatError::Corrupt { at: "stats run section attno" });
            }
        }
        attnos.sort_unstable();
        if nfacts != 1 || nparts_sec != 1 || nroot > 1 || npartext > 1 || attnos.windows(2).any(|w| w[0] == w[1]) {
            return Err(FormatError::Corrupt { at: "stats run section set" });
        }
        let pb = file.section(file.find(SECTION_RUN_PARTS, 0).expect("counted"))?;
        if !pb.len().is_multiple_of(BANKSTATS_PARTIDENT_LEN) {
            return Err(FormatError::Corrupt { at: "stats run parts length" });
        }
        let mut c = Cur::new(pb);
        let parts = (0..pb.len() / BANKSTATS_PARTIDENT_LEN).map(|_| PartIdent::decode_from(&mut c)).collect::<FormatResult<Vec<_>>>()?;
        if parts.len() as u64 != u64::from(want.nparts) {
            return Err(FormatError::Corrupt { at: "stats run nparts vs ref" });
        }
        if parts.windows(2).any(|w| w[0].part_no >= w[1].part_no) {
            return Err(FormatError::Corrupt { at: "stats run parts order" });
        }
        let root = match file.find(SECTION_RUN_ROOT, 0) {
            None => None,
            Some(i) => {
                let refs = decode_refs(file.section(i)?)?;
                let ordered = refs.windows(2).all(|w| w[0].key() < w[1].key());
                if !ordered || refs.iter().any(|r| r.key() >= want.key() || r.nparts == 0) {
                    return Err(FormatError::Corrupt { at: "stats run root list" });
                }
                Some(refs)
            }
        };
        Ok(RunFile { gen: file.gen, publisher_fxid: file.publisher_fxid, parts, root, file })
    }

    /// True when the run was written for a table of this schema fingerprint.
    pub fn binds_schema(&self, schema_fingerprint: u64) -> bool {
        self.file.manifest_crc == schema_crc(schema_fingerprint)
    }

    /// The run's facts image; one whose part count is not the run's is `Corrupt`.
    pub fn facts(&self) -> FormatResult<FactsImage> {
        let img = FactsImage::decode(self.file.section(self.file.find(SECTION_FACTS, 0).expect("parsed"))?)?;
        if img.header.parts != self.parts.len() as u64 {
            return Err(FormatError::Corrupt { at: "stats run facts parts" });
        }
        Ok(img)
    }

    /// Every column's answers payload, attno ascending, each validated over the run's parts.
    pub fn answers(&self) -> FormatResult<Vec<(u32, Vec<u8>)>> {
        let mut out = Vec::new();
        for (attno, i) in self.file.all(SECTION_ANSWERS) {
            let b = self.file.section(i)?;
            crate::answers::ColumnAnswers::parse(b, self.parts.len())?;
            out.push((attno, crate::try_to_vec(b, "stats run answers")?));
        }
        out.sort_unstable_by_key(|(a, _)| *a);
        Ok(out)
    }

    /// The part extension records, (part_no, tag) ascending, each naming one of the run's parts.
    pub fn partext(&self) -> FormatResult<Vec<(u32, u16, Vec<u8>)>> {
        let at = "stats run partext";
        let Some(i) = self.file.find(SECTION_RUN_PARTEXT, 0) else {
            return Ok(Vec::new());
        };
        let mut c = Cur::new(self.file.section(i)?);
        let mut out: Vec<(u32, u16, Vec<u8>)> = Vec::new();
        while c.remaining() > 0 {
            if c.remaining() < PARTEXT_HEAD_LEN {
                return Err(FormatError::Truncated { at });
            }
            let part_no = c.u32(at)?;
            let tag = c.u16(at)?;
            if c.u16(at)? != 0 {
                return Err(FormatError::Corrupt { at });
            }
            let len = c.u32(at)? as usize;
            let bytes = crate::try_to_vec(c.take(len, at)?, at)?;
            let ordered = out.last().is_none_or(|l| (l.0, l.1) < (part_no, tag));
            if !ordered || self.parts.binary_search_by_key(&part_no, |p| p.part_no).is_err() {
                return Err(FormatError::Corrupt { at });
            }
            out.push((part_no, tag, bytes));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answers::encode_column;
    use crate::facts::{ColFacts, FactsHeader};
    use crate::sections::SECTIONS_HEADER_LEN;

    fn ident(part_no: u32) -> PartIdent {
        PartIdent { part_no, granule_count: 1, band_count: 1, rows: 100 + u64::from(part_no), footer_off: 4096 }
    }

    fn facts(parts: u64) -> FactsImage {
        FactsImage {
            header: FactsHeader { gen: 7, publisher_fxid: 70, schema_fingerprint: 5, rows: 300, bytes: 900, granules: parts, parts },
            cols: vec![ColFacts { attno: 1, class: 1, width: 8, nonnull: 300, ..ColFacts::default() }],
        }
    }

    struct Built {
        bytes: Vec<u8>,
        rref: RunRef,
    }

    fn build(gen: u64, seq: u32, part_nos: &[u32], root: Option<&[RunRef]>, partext: &[(u32, u16, &[u8])]) -> Built {
        let parts: Vec<PartIdent> = part_nos.iter().map(|&n| ident(n)).collect();
        let f = facts(parts.len() as u64);
        let bodies: Vec<Vec<u8>> = parts.iter().map(|p| vec![p.part_no as u8; 80]).collect();
        let body_refs: Vec<&[u8]> = bodies.iter().map(|b| &b[..]).collect();
        let digests: Vec<Option<[u8; 24]>> = parts.iter().map(|p| Some([p.part_no as u8 + 1; 24])).collect();
        let col1 = encode_column(&body_refs, &digests).unwrap();
        let col3 = encode_column(&vec![&[][..]; parts.len()], &vec![None; parts.len()]).unwrap();
        let answers: Vec<(u32, &[u8])> = vec![(1, &col1[..]), (3, &col3[..])];
        let bytes = encode_run(&RunImage { gen, publisher_fxid: gen * 10, schema_fingerprint: 5, parts: &parts, facts: &f, answers: &answers, root, partext }).expect("encode_run");
        let rref = RunRef::of(gen, seq, parts.len() as u32, &bytes);
        Built { bytes, rref }
    }

    #[test]
    fn a_run_round_trips_through_its_ref() {
        let root = [RunRef { gen: 3, seq: 0, nparts: 256, len: 9000, crc: 11 }, RunRef { gen: 7, seq: 0, nparts: 16, len: 800, crc: 12 }];
        let b = build(7, 1, &[4, 9, 12], Some(&root[..]), &[(4, 1, &b"birth"[..]), (4, 2, &b""[..]), (12, 1, &b"x"[..])]);
        let r = RunFile::parse(b.bytes.clone(), &b.rref).unwrap();
        assert_eq!((r.gen, r.publisher_fxid), (7, 70));
        assert_eq!(r.parts, vec![ident(4), ident(9), ident(12)]);
        assert_eq!(r.root.as_deref(), Some(&root[..]));
        assert!(r.binds_schema(5) && !r.binds_schema(6));
        assert_eq!(r.facts().unwrap(), facts(3));
        let answers = r.answers().unwrap();
        assert_eq!(answers.iter().map(|a| a.0).collect::<Vec<_>>(), vec![1, 3]);
        let col = crate::answers::ColumnAnswers::parse(&answers[0].1, 3).unwrap();
        assert_eq!(col.stats_body(1), Some(&[9u8; 80][..]));
        assert_eq!(col.digest(2), Some(&[13u8; 24][..]));
        assert_eq!(crate::answers::ColumnAnswers::parse(&answers[1].1, 3).unwrap().stats_body(0), None);
        assert_eq!(r.partext().unwrap(), vec![(4, 1, b"birth".to_vec()), (4, 2, Vec::new()), (12, 1, b"x".to_vec())]);
        assert_eq!(decode_refs(&encode_refs(&root)).unwrap(), root.to_vec());
        assert!(decode_refs(&[0u8; 31]).is_err());
    }

    #[test]
    fn a_run_without_a_root_or_extensions_and_a_zero_part_root_carrier() {
        let plain = build(4, 0, &[1, 2], None, &[]);
        let r = RunFile::parse(plain.bytes, &plain.rref).unwrap();
        assert!(r.root.is_none());
        assert!(r.partext().unwrap().is_empty());
        let listed = [plain.rref];
        let carrier = build(5, 0, &[], Some(&listed[..]), &[]);
        let c = RunFile::parse(carrier.bytes, &carrier.rref).unwrap();
        assert!(c.parts.is_empty());
        assert_eq!(c.root.as_deref(), Some(&listed[..]));
        assert_eq!(c.facts().unwrap().header.parts, 0);
        assert_eq!(c.answers().unwrap().len(), 2);
    }

    #[test]
    fn a_run_read_by_another_ref_is_refused() {
        let b = build(7, 1, &[4, 9], None, &[]);
        let bad_refs = [
            RunRef { len: b.rref.len + 1, ..b.rref },
            RunRef { crc: b.rref.crc ^ 1, ..b.rref },
            RunRef { gen: 8, ..b.rref },
            RunRef { nparts: 3, ..b.rref },
        ];
        for want in &bad_refs {
            assert!(RunFile::parse(b.bytes.clone(), want).is_err(), "{want:?}");
        }
        let mut flipped = b.bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(RunFile::parse(flipped, &b.rref).is_err(), "a flipped byte fails the whole-file crc");
        let mut other_magic = b.bytes.clone();
        other_magic[0] ^= 1;
        let r = RunRef::of(7, 1, 2, &other_magic);
        assert!(matches!(RunFile::parse(other_magic, &r), Err(FormatError::BadMagic { .. })));
    }

    /// A run assembled from raw sections, with its matching ref: what a corrupt writer could produce.
    fn raw(sections: &[(u32, u32, &[u8])], nparts: u32) -> FormatResult<RunFile> {
        let bytes = assemble_bound(RUN_MAGIC, RUN_VERSION, 7, 70, schema_crc(5), sections)?;
        let r = RunRef::of(7, 1, nparts, &bytes);
        RunFile::parse(bytes, &r)
    }

    fn idents(part_nos: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for &n in part_nos {
            ident(n).encode_into(&mut out);
        }
        out
    }

    #[test]
    fn corrupt_parts_sections_and_roots_are_refused() {
        let f = facts(2).encode().unwrap();
        let good = idents(&[1, 2]);
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f)], 2).is_ok());
        for parts in [idents(&[2, 1]), idents(&[2, 2])] {
            assert!(raw(&[(SECTION_RUN_PARTS, 0, &parts), (SECTION_FACTS, 0, &f)], 2).is_err(), "unsorted or duplicate parts");
        }
        let mut padded = good.clone();
        padded[12] = 1;
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &padded), (SECTION_FACTS, 0, &f)], 2).is_err(), "a nonzero ident pad");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good[..40]), (SECTION_FACTS, 0, &f)], 2).is_err(), "a ragged parts section");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (9, 0, b"")], 2).is_err(), "an unknown kind");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good)], 2).is_err(), "no facts");
        assert!(raw(&[(SECTION_FACTS, 0, &f)], 0).is_err(), "no parts section");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_FACTS, 0, &f)], 2).is_err(), "two facts");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 1, &f)], 2).is_err(), "facts under an attno");
        let col = encode_column(&[&[1u8][..], &[2u8][..]], &[None, None]).unwrap();
        let two_answers = [(SECTION_RUN_PARTS, 0, &good[..]), (SECTION_FACTS, 0, &f[..]), (SECTION_ANSWERS, 1, &col[..]), (SECTION_ANSWERS, 1, &col[..])];
        assert!(raw(&two_answers, 2).is_err(), "two answers sections of one attno");
        let short = [(SECTION_RUN_PARTS, 0, &good[..]), (SECTION_FACTS, 0, &f[..]), (SECTION_ANSWERS, 1, &col[..8])];
        assert!(raw(&short, 2).unwrap().answers().is_err(), "an answers payload short of the run's parts");
        let f3 = facts(3).encode().unwrap();
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f3)], 2).unwrap().facts().is_err(), "facts over other parts");
        let r = |gen, seq, nparts| RunRef { gen, seq, nparts, len: 64, crc: 1 };
        for (refs, why) in [
            (vec![r(3, 0, 5), r(2, 0, 5)], "unsorted"),
            (vec![r(3, 0, 5), r(3, 0, 5)], "duplicate"),
            (vec![r(7, 1, 5)], "names the carrier itself"),
            (vec![r(8, 0, 5)], "a later generation"),
            (vec![r(3, 0, 0)], "a run of no parts"),
        ] {
            let root = encode_refs(&refs);
            assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_RUN_ROOT, 0, &root)], 2).is_err(), "{why}");
        }
        let ok_root = encode_refs(&[r(3, 0, 5), r(7, 0, 16)]);
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_RUN_ROOT, 0, &ok_root)], 2).is_ok());
        let mut bad_pad = ok_root.clone();
        bad_pad[31] = 1;
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_RUN_ROOT, 0, &bad_pad)], 2).is_err(), "a nonzero ref pad");
        assert!(raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_RUN_ROOT, 0, &ok_root[..20])], 2).is_err(), "a ragged root");
    }

    #[test]
    fn corrupt_part_extensions_are_refused() {
        let f = facts(2).encode().unwrap();
        let good = idents(&[1, 2]);
        let ext = |recs: &[(u32, u16, u16, &[u8])]| {
            let mut out = Vec::new();
            for (p, t, pad, b) in recs {
                put_u32(&mut out, *p);
                put_u16(&mut out, *t);
                put_u16(&mut out, *pad);
                put_u32(&mut out, b.len() as u32);
                out.extend_from_slice(b);
            }
            out
        };
        let read = |x: &[u8]| raw(&[(SECTION_RUN_PARTS, 0, &good), (SECTION_FACTS, 0, &f), (SECTION_RUN_PARTEXT, 0, x)], 2).unwrap().partext();
        assert_eq!(read(&ext(&[(1, 1, 0, b"a"), (2, 1, 0, b"")])).unwrap().len(), 2);
        assert!(read(&ext(&[(3, 1, 0, b"a")])).is_err(), "a part the run does not hold");
        assert!(read(&ext(&[(2, 1, 0, b"a"), (1, 1, 0, b"")])).is_err(), "unsorted");
        assert!(read(&ext(&[(1, 1, 0, b"a"), (1, 1, 0, b"")])).is_err(), "a duplicate (part_no, tag)");
        assert!(read(&ext(&[(1, 1, 7, b"a")])).is_err(), "a nonzero pad");
        let full = ext(&[(1, 1, 0, b"abc")]);
        assert!(read(&full[..full.len() - 1]).is_err(), "a truncated record");
        assert!(read(&full[..5]).is_err(), "a truncated head");
    }

    #[test]
    fn the_run_header_is_the_sections_header_under_its_own_magic() {
        let b = build(7, 1, &[4], None, &[]);
        assert_eq!(&b.bytes[..4], b"PRCR");
        assert_eq!(u32::from_le_bytes(b.bytes[4..8].try_into().unwrap()), RUN_VERSION);
        assert_eq!(u64::from_le_bytes(b.bytes[8..16].try_into().unwrap()), 7);
        assert!(b.bytes.len() > SECTIONS_HEADER_LEN);
        assert!(SectionsFile::parse(b.bytes).is_err(), "a run is not a sections file");
    }
}
