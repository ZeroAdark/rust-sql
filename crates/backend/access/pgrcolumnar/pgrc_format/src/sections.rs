//! The generation's sections file, `sections-<gen>.pgrcx`: what the
//! catalog serves besides the part list — section 1 = the planner facts
//! accumulator (`facts::FactsImage`), sections 2.. = one per column, the
//! part-grain answers (`answers`). Written by the publisher after the
//! manifest and before `CURRENT`, bound to its manifest by (gen,
//! publisher_fxid, the manifest's trailing crc); a generation without it
//! (a legacy bank, a publish that could not fold) is completed once by the
//! catalog. The manifest file itself is untouched — byte-identical to
//! version 1, so every reader that ships (main's `Manifest::decode`: whole
//! file, exact length, trailing crc) parses it unchanged.
//!
//! ```text
//! header (40 B): magic "PRCS" u32, version u32 = 2, gen u64, publisher_fxid u64,
//!                manifest_crc u32, nsections u32, reserved u64
//! (version 2, 2026-09-13: section 1's zone maps are compact — `facts.rs`,
//! `zonemap.rs`; a version-1 file is re-completed once at startup)
//! entry (32 B) × nsections: kind u32, attno u32, off u64, len u64, crc u32, pad u32
//! table crc u32 (crc32c over header + entries)
//! payloads, 8-aligned, each crc32c'd by its entry
//! ```

use crate::manifest::{Manifest, MANIFEST_HEADER_LEN, PART_RECORD_LEN};
use crate::wire::{crc32c, put_u32, put_u64, Cur};
use crate::part::SectionBuf;
use crate::{FormatError, FormatResult};
use memledger::charge::Owner;
use memledger::{ChargedVec, Kind};

pub const SECTIONS_MAGIC: u32 = u32::from_le_bytes(*b"PRCS");
pub const SECTIONS_VERSION: u32 = 2;
pub const SECTIONS_HEADER_LEN: usize = 40;
pub const SECTION_ENTRY_LEN: usize = 32;
pub const SECTION_FACTS: u32 = 1;
pub const SECTION_ANSWERS: u32 = 2;
/// Bytes a manifest reader asks for first: a whole manifest of up to 1,000 parts in one read.
pub const FRONT_READ_LEN: usize = 96 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionEntry {
    pub kind: u32,
    pub attno: u32,
    pub off: u64,
    pub len: u64,
    pub crc: u32,
}

/// The byte length of a manifest file for `part_count` parts.
pub const fn section0_len(part_count: u32) -> usize {
    MANIFEST_HEADER_LEN + part_count as usize * PART_RECORD_LEN + 4
}

/// The part count a manifest header states, from the file's first 72 bytes.
pub fn front_part_count(bytes: &[u8]) -> FormatResult<u32> {
    if bytes.len() < MANIFEST_HEADER_LEN {
        return Err(FormatError::Truncated { at: "Manifest" });
    }
    Ok(u32::from_le_bytes(bytes[56..60].try_into().expect("len 4")))
}

/// The length of the manifest at the front of `bytes`, its extension block included. With the block flagged and its
/// length word not yet in `bytes`, the length up to that word: read that much and ask again.
pub fn front_len(bytes: &[u8]) -> FormatResult<usize> {
    let s0 = section0_len(front_part_count(bytes)?);
    let flags = u32::from_le_bytes(bytes[64..68].try_into().expect("len 4"));
    if flags & crate::manifest::MANIFESTF_EXT == 0 || bytes.len() < s0 {
        return Ok(s0);
    }
    Ok(s0 + 4 + u32::from_le_bytes(bytes[s0 - 4..s0].try_into().expect("len 4")) as usize)
}

/// The front of a manifest file through `read(len)`, a prefix read: the bytes (at least the manifest) and its length.
pub fn read_front_bytes<E: From<FormatError>>(read: &mut dyn FnMut(usize) -> Result<Vec<u8>, E>) -> Result<(Vec<u8>, usize), E> {
    let mut bytes = read(FRONT_READ_LEN)?;
    loop {
        let need = front_len(&bytes)?;
        if bytes.len() >= need {
            return Ok((bytes, need));
        }
        let more = read(need)?;
        if more.len() <= bytes.len() {
            return Err(FormatError::Truncated { at: "Manifest" }.into());
        }
        bytes = more;
    }
}

/// The manifest decoded from the front of its file (`bytes` may be a prefix covering it); returns its length too.
pub fn decode_front(bytes: &[u8]) -> FormatResult<(Manifest, usize)> {
    let s0 = front_len(bytes)?;
    if bytes.len() < s0 {
        return Err(FormatError::Truncated { at: "Manifest" });
    }
    Ok((Manifest::decode(&bytes[..s0])?, s0))
}

/// The manifest's own trailing crc (what `CURRENT` and the sections file name).
pub fn manifest_crc(manifest_bytes: &[u8]) -> u32 {
    u32::from_le_bytes(manifest_bytes[manifest_bytes.len() - 4..].try_into().expect("len 4"))
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Assemble a sections file for `manifest` (whose file crc is `mcrc`).
pub fn assemble(manifest: &Manifest, mcrc: u32, sections: &[(u32, u32, &[u8])]) -> FormatResult<Vec<u8>> {
    assemble_bound(SECTIONS_MAGIC, SECTIONS_VERSION, manifest.header.gen, manifest.header.publisher_fxid, mcrc, sections)
}

/// Assemble a file in the sections container under `magic` / `version`: the header names `(gen, fxid, bind_crc)`
/// (`bind_crc` is the manifest crc for a sections file, what the artifact binds to for others, e.g. `statsrun`).
pub fn assemble_bound(magic: u32, version: u32, gen: u64, fxid: u64, bind_crc: u32, sections: &[(u32, u32, &[u8])]) -> FormatResult<Vec<u8>> {
    let mut out = Vec::new(); // alloc: Node charged caller (assemble_for charges it)
    assemble_in((magic, version, gen, fxid, bind_crc), sections, &mut SectionBuf::Plain(&mut out))?;
    Ok(out)
}

/// Wave 3 A4 (cvformat): [`assemble`] into a `ChargedVec` on `owner` (the publishing statement; `None`: uncharged). The
/// whole file is reserved, and so charged, before a byte is written; it never grows after. The bytes are [`assemble`]'s.
pub fn assemble_for(
    manifest: &Manifest,
    mcrc: u32,
    sections: &[(u32, u32, &[u8])],
    owner: Option<(Owner, Kind)>,
) -> FormatResult<ChargedVec<u8>> {
    let mut out = match owner {
        Some((o, k)) => ChargedVec::new(Some(o), k),
        None => ChargedVec::none(),
    };
    let head = (SECTIONS_MAGIC, SECTIONS_VERSION, manifest.header.gen, manifest.header.publisher_fxid, mcrc);
    assemble_in(head, sections, &mut SectionBuf::Charged(core::mem::ManuallyDrop::new(out.vec_mut())))?;
    Ok(out)
}

/// The container body: `head` = (magic, version, gen, fxid, bind_crc) (line: `assemble_bound`'s header; trunk wave 3
/// A9: one body over a [`SectionBuf`]).
fn assemble_in(head: (u32, u32, u64, u64, u32), sections: &[(u32, u32, &[u8])], buf: &mut SectionBuf<'_>) -> FormatResult<()> {
    let (magic, version, gen, fxid, bind_crc) = head;
    let table_len = SECTIONS_HEADER_LEN + sections.len() * SECTION_ENTRY_LEN + 4;
    let mut off = align8(table_len);
    let mut entries: Vec<SectionEntry> = crate::try_vec(sections.len(), "sections")?;
    for (kind, attno, bytes) in sections {
        entries.push(SectionEntry { kind: *kind, attno: *attno, off: off as u64, len: bytes.len() as u64, crc: crc32c(bytes) });
        off = align8(off + bytes.len());
    }
    // The whole file at once (each section 8-aligned after the table): it never grows after this.
    buf.vec().try_reserve_exact(off).map_err(|_| FormatError::OutOfMemory { bytes: off })?; // alloc: Node charged buf.owner()
    buf.sync();
    let out = buf.vec();
    put_u32(out, magic); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u32(out, version); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u64(out, gen); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u64(out, fxid); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u32(out, bind_crc); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u32(out, entries.len() as u32); // alloc: Node charged buf.owner() (reserved: no growth)
    put_u64(out, 0); // alloc: Node charged buf.owner() (reserved: no growth)
    debug_assert_eq!(out.len(), SECTIONS_HEADER_LEN);
    for e in &entries {
        put_u32(out, e.kind); // alloc: Node charged buf.owner() (reserved: no growth)
        put_u32(out, e.attno); // alloc: Node charged buf.owner() (reserved: no growth)
        put_u64(out, e.off); // alloc: Node charged buf.owner() (reserved: no growth)
        put_u64(out, e.len); // alloc: Node charged buf.owner() (reserved: no growth)
        put_u32(out, e.crc); // alloc: Node charged buf.owner() (reserved: no growth)
        put_u32(out, 0); // alloc: Node charged buf.owner() (reserved: no growth)
    }
    let tcrc = crc32c(out);
    put_u32(out, tcrc); // alloc: Node charged buf.owner() (reserved: no growth)
    for (i, (_, _, bytes)) in sections.iter().enumerate() {
        crate::try_resize(out, entries[i].off as usize, 0, "sections")?; // alloc: Node charged buf.owner() (reserved: no growth)
        out.extend_from_slice(bytes); // alloc: Node charged buf.owner() (reserved: no growth)
    }
    buf.sync();
    Ok(())
}

/// A sections file in memory: the key it names and its sections, served by index with their crc checked.
pub struct SectionsFile {
    pub bytes: Box<[u8]>,
    pub gen: u64,
    pub publisher_fxid: u64,
    pub manifest_crc: u32,
    pub entries: Vec<SectionEntry>,
}

impl SectionsFile {
    pub fn parse(bytes: Vec<u8>) -> FormatResult<SectionsFile> {
        Self::parse_with(bytes, SECTIONS_MAGIC, SECTIONS_VERSION)
    }

    /// Parse a file in the sections container written under `magic` / `version` (`assemble_bound`).
    pub fn parse_with(bytes: Vec<u8>, magic: u32, version: u32) -> FormatResult<SectionsFile> {
        let at = "sections file";
        if bytes.len() < SECTIONS_HEADER_LEN + 4 {
            return Err(FormatError::Truncated { at });
        }
        let mut c = Cur::new(&bytes);
        if c.u32(at)? != magic {
            return Err(FormatError::BadMagic { at });
        }
        let v = c.u32(at)?;
        if v != version {
            return Err(FormatError::BadVersion { at, got: v });
        }
        let gen = c.u64(at)?;
        let publisher_fxid = c.u64(at)?;
        let mcrc = c.u32(at)?;
        let n = c.u32(at)? as usize;
        let _ = c.u64(at)?;
        if n > 1 << 20 {
            return Err(FormatError::Corrupt { at: "sections count" });
        }
        let tlen = SECTIONS_HEADER_LEN + n * SECTION_ENTRY_LEN + 4;
        if bytes.len() < tlen {
            return Err(FormatError::Truncated { at });
        }
        let stored = u32::from_le_bytes(bytes[tlen - 4..tlen].try_into().expect("len 4"));
        if crc32c(&bytes[..tlen - 4]) != stored {
            return Err(FormatError::CrcMismatch { at: "sections table" });
        }
        let mut entries = crate::try_vec(n, "sections")?;
        for _ in 0..n {
            let e = SectionEntry { kind: c.u32(at)?, attno: c.u32(at)?, off: c.u64(at)?, len: c.u64(at)?, crc: c.u32(at)? };
            let _ = c.u32(at)?;
            if e.off.checked_add(e.len).map_or(true, |end| end > bytes.len() as u64) {
                return Err(FormatError::Truncated { at: "section" });
            }
            entries.push(e);
        }
        Ok(SectionsFile { bytes: bytes.into_boxed_slice(), gen, publisher_fxid, manifest_crc: mcrc, entries })
    }

    /// True when the file is this manifest's.
    pub fn names(&self, manifest: &Manifest, mcrc: u32) -> bool {
        self.gen == manifest.header.gen && self.publisher_fxid == manifest.header.publisher_fxid && self.manifest_crc == mcrc
    }

    /// Section `i`'s bytes, crc verified.
    pub fn section(&self, i: usize) -> FormatResult<&[u8]> {
        let e = self.entries.get(i).ok_or(FormatError::Corrupt { at: "section index" })?;
        let b = &self.bytes[e.off as usize..(e.off + e.len) as usize];
        if crc32c(b) != e.crc {
            return Err(FormatError::CrcMismatch { at: "section" });
        }
        Ok(b)
    }

    pub fn find(&self, kind: u32, attno: u32) -> Option<usize> {
        self.entries.iter().position(|e| e.kind == kind && e.attno == attno)
    }

    /// Every section of `kind`, as (attno, index).
    pub fn all(&self, kind: u32) -> impl Iterator<Item = (u32, usize)> + '_ {
        self.entries.iter().enumerate().filter(move |(_, e)| e.kind == kind).map(|(i, e)| (e.attno, i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestHeader, MANIFEST_MAGIC};
    use crate::FORMAT_VERSION;

    fn manifest() -> Manifest {
        Manifest::new(ManifestHeader { gen: 3, prev_gen: 2, publisher_fxid: 9, relfilenumber: 7, schema_fingerprint: 1, magic: MANIFEST_MAGIC, format_version: FORMAT_VERSION, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 }, Vec::new())
    }

    #[test]
    fn a_manifest_with_an_extension_block_decodes_from_the_front_of_its_file() {
        let mut ext = Vec::new();
        crate::manifest::put_ext_record(&mut ext, 1, &[7u8; 300]);
        let m = manifest().with_ext(ext);
        let mut file = m.encode();
        let len = file.len();
        file.extend_from_slice(&[0u8; 64]);
        assert_eq!(front_len(&file).unwrap(), len);
        assert_eq!(decode_front(&file).unwrap(), (m.clone(), len));
        let mut reads = Vec::new();
        let (bytes, n) = read_front_bytes::<FormatError>(&mut |want| {
            let want = if reads.is_empty() { MANIFEST_HEADER_LEN + 2 } else { want };
            reads.push(want);
            Ok(file[..want.min(file.len())].to_vec())
        })
        .unwrap();
        assert_eq!((n, decode_front(&bytes).unwrap().0), (len, m));
        assert_eq!(reads.len(), 3, "a short first read, then the parts, then the extension block");
    }

    #[test]
    fn a_sections_file_names_its_manifest_and_serves_its_sections_crc_checked() {
        let m = manifest();
        let mbytes = m.encode();
        let mcrc = manifest_crc(&mbytes);
        assert_eq!(decode_front(&mbytes).unwrap().1, mbytes.len());
        let facts = b"facts bytes".to_vec();
        let col = vec![7u8; 13];
        let file = assemble(&m, mcrc, &[(SECTION_FACTS, 0, &facts), (SECTION_ANSWERS, 4, &col)]).unwrap();
        let f = SectionsFile::parse(file.clone()).unwrap();
        assert!(f.names(&m, mcrc));
        assert!(!f.names(&m, mcrc ^ 1), "another manifest's crc: not this generation's file");
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.section(f.find(SECTION_FACTS, 0).unwrap()).unwrap(), &facts[..]);
        assert_eq!(f.section(f.find(SECTION_ANSWERS, 4).unwrap()).unwrap(), &col[..]);
        assert!(f.find(SECTION_ANSWERS, 5).is_none());
        assert_eq!(f.all(SECTION_ANSWERS).collect::<Vec<_>>(), vec![(4, 1)]);
        let mut bad = file.clone();
        let off = f.entries[1].off as usize;
        bad[off] ^= 1;
        assert!(SectionsFile::parse(bad).unwrap().section(1).is_err());
        let mut bad_table = file;
        bad_table[SECTIONS_HEADER_LEN + 8] ^= 1;
        assert!(SectionsFile::parse(bad_table).is_err());
    }
}
