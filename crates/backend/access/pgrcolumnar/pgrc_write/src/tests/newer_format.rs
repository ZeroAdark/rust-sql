//! End-of-recovery cleanup and a manifest this binary cannot read (SEAL-OFFHEAD downgrade, rule (d)): a manifest on
//! CURRENT's chain whose trailing crc matches its bytes (and CURRENT's length and crc) but that does not decode was
//! written by a newer format, not torn. Cleanup refuses the directory and changes nothing. Before this rule it fell
//! back to the newest generation it could decode and deleted everything above it (the committed rows those
//! generations held were lost). A torn manifest still falls back as before.

use super::*;
use crate::cleanup::cleanup_dir;
use crate::publish::TxnVerdict;
use crate::walsink::NoWal;
use crate::WriteError;
use pgrc_format::dirlayout::{manifest_file_name, CURRENT_FILE_NAME};
use pgrc_format::manifest::CommitPointer;
use pgrc_format::sections::read_front_bytes;
use pgrc_format::wire::crc32c;

fn bulk(vfs: &mut MemVfs, fxid: u64, probe: &Probe) -> u64 {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, 10, |i| Some((fxid * 1000 + i) as i64));
    finish_and_publish(&mut w, vfs, &mut kit, probe).gen
}

fn put(vfs: &mut MemVfs, name: &str, bytes: &[u8]) {
    let p = format!("{DIR}/{name}");
    let fd = vfs.create_rw(&p).expect("create");
    vfs.pwrite_at(&fd, 0, bytes).expect("write");
    vfs.close_file(fd).expect("close");
}

/// Generation `gen`'s manifest rewritten with an unknown header flag bit: `fix_crc` re-seals its trailing crc (a
/// newer binary's intact manifest), else the crc is left stale (a torn write). CURRENT follows when it names `gen`.
fn unknown_flag(vfs: &mut MemVfs, gen: u64, fix_crc: bool) {
    let name = manifest_file_name(gen);
    let p = format!("{DIR}/{name}");
    let (mut b, s0) = read_front_bytes(&mut |len| vfs.read_prefix(&p, len)).expect("front");
    b.truncate(s0);
    let flags = u32::from_le_bytes(b[64..68].try_into().unwrap()) | 0x8000;
    b[64..68].copy_from_slice(&flags.to_le_bytes());
    if fix_crc {
        let crc = crc32c(&b[..s0 - 4]);
        b[s0 - 4..].copy_from_slice(&crc.to_le_bytes());
        let cur = CommitPointer::decode(&vfs.read_full(&format!("{DIR}/{CURRENT_FILE_NAME}")).unwrap()).unwrap();
        if cur.gen == gen {
            put(vfs, CURRENT_FILE_NAME, &CommitPointer::new(gen, s0 as u64, crc).encode());
        }
    }
    put(vfs, &name, &b);
}

fn snapshot(vfs: &mut MemVfs) -> Vec<(String, Vec<u8>)> {
    let mut names = vfs.list_dir(DIR).unwrap();
    names.sort();
    names.into_iter().map(|n| (n.clone(), vfs.read_full(&format!("{DIR}/{n}")).unwrap())).collect()
}

#[test]
fn cleanup_refuses_an_intact_manifest_it_cannot_decode_and_changes_nothing() {
    let committed = Probe::new(TxnVerdict::Committed);
    // CURRENT names the unreadable generation
    let mut vfs = mem_with_dir();
    let g1 = bulk(&mut vfs, 50, &committed);
    let g2 = bulk(&mut vfs, 51, &committed);
    unknown_flag(&mut vfs, g2, true);
    let before = snapshot(&mut vfs);
    let r = cleanup_dir(&mut vfs, &mut NoWal, DIR, &committed);
    assert!(matches!(r, Err(WriteError::ManifestChain { .. })), "refused: {r:?}");
    assert!(r.unwrap_err().to_string().contains("cannot decode"), "a clear error");
    assert_eq!(snapshot(&mut vfs), before, "CURRENT and every file untouched");
    assert!(g1 < g2);

    // below CURRENT: an aborted writer's readable generation on top of the unreadable committed one
    let mut vfs = mem_with_dir();
    bulk(&mut vfs, 50, &committed);
    let g2 = bulk(&mut vfs, 51, &committed);
    unknown_flag(&mut vfs, g2, true);
    let aborted = Probe::new(TxnVerdict::Committed).set(52, TxnVerdict::Aborted);
    // the writer's publish reads its base through the unreadable manifest: stage it by hand as generation g2 + 1
    let p = format!("{DIR}/{}", manifest_file_name(g2));
    let (b, s0) = read_front_bytes(&mut |len| vfs.read_prefix(&p, len)).unwrap();
    let mut m3 = b[..s0].to_vec();
    m3[0..8].copy_from_slice(&(g2 + 1).to_le_bytes()); // gen
    m3[8..16].copy_from_slice(&g2.to_le_bytes()); // prev_gen
    m3[16..24].copy_from_slice(&52u64.to_le_bytes()); // publisher_fxid
    let flags = u32::from_le_bytes(m3[64..68].try_into().unwrap()) & !0x8000;
    m3[64..68].copy_from_slice(&flags.to_le_bytes());
    let crc = crc32c(&m3[..s0 - 4]);
    m3[s0 - 4..].copy_from_slice(&crc.to_le_bytes());
    assert!(pgrc_format::manifest::Manifest::decode(&m3).is_ok(), "the staged generation decodes");
    put(&mut vfs, &manifest_file_name(g2 + 1), &m3);
    put(&mut vfs, CURRENT_FILE_NAME, &CommitPointer::new(g2 + 1, s0 as u64, crc).encode());
    let before = snapshot(&mut vfs);
    assert!(matches!(cleanup_dir(&mut vfs, &mut NoWal, DIR, &aborted), Err(WriteError::ManifestChain { .. })), "refused through the chain");
    assert_eq!(snapshot(&mut vfs), before);
}

#[test]
fn a_torn_manifest_still_falls_back_to_the_last_generation() {
    let committed = Probe::new(TxnVerdict::Committed);
    let mut vfs = mem_with_dir();
    let g1 = bulk(&mut vfs, 50, &committed);
    let g2 = bulk(&mut vfs, 51, &committed);
    unknown_flag(&mut vfs, g2, false);
    let rep = cleanup_dir(&mut vfs, &mut NoWal, DIR, &committed).expect("a torn manifest is crash residue");
    assert_eq!((rep.effective_gen, rep.current_repointed), (g1, true));
}

/// Generation `gen`'s manifest rewritten in a layout this binary predates: every part record 16 bytes longer (IX0 L0
/// grew them from 64 to 80 B, q019g), its crc re-sealed and CURRENT following when it names `gen`. This binary computes
/// the wrong length for it, so its prefix read or decode fails, yet the file is intact.
fn longer_records(vfs: &mut MemVfs, gen: u64) {
    use pgrc_format::manifest::{MANIFEST_HEADER_LEN, PART_RECORD_LEN};
    let name = manifest_file_name(gen);
    let p = format!("{DIR}/{name}");
    let b = vfs.read_full(&p).unwrap();
    let n = u32::from_le_bytes(b[56..60].try_into().unwrap()) as usize;
    assert!(n > 0, "a manifest with parts");
    let mut out = b[..MANIFEST_HEADER_LEN].to_vec();
    for i in 0..n {
        let at = MANIFEST_HEADER_LEN + i * PART_RECORD_LEN;
        out.extend_from_slice(&b[at..at + PART_RECORD_LEN]);
        out.extend_from_slice(&[0u8; 16]);
    }
    out.extend_from_slice(&b[MANIFEST_HEADER_LEN + n * PART_RECORD_LEN..b.len() - 4]);
    let crc = crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    let cur = CommitPointer::decode(&vfs.read_full(&format!("{DIR}/{CURRENT_FILE_NAME}")).unwrap()).unwrap();
    if cur.gen == gen {
        put(vfs, CURRENT_FILE_NAME, &CommitPointer::new(gen, out.len() as u64, crc).encode());
    }
    put(vfs, &name, &out);
}

/// q019g: a binary that computes the wrong length for an intact manifest (longer records than it knows) must refuse,
/// not fall back and delete. The header and both crcs (the file's own, CURRENT's) check out; only the length this
/// binary derives from the part count disagrees.
#[test]
fn cleanup_refuses_an_intact_manifest_with_longer_records() {
    let committed = Probe::new(TxnVerdict::Committed);
    let mut vfs = mem_with_dir();
    bulk(&mut vfs, 50, &committed);
    let g2 = bulk(&mut vfs, 51, &committed);
    longer_records(&mut vfs, g2);
    let before = snapshot(&mut vfs);
    let r = cleanup_dir(&mut vfs, &mut NoWal, DIR, &committed);
    assert!(matches!(r, Err(WriteError::ManifestChain { .. })), "refused: {r:?}");
    assert_eq!(snapshot(&mut vfs), before, "CURRENT and every file untouched");
    // the same file torn (its crc stale) is still crash residue
    let mut vfs = mem_with_dir();
    let g1 = bulk(&mut vfs, 50, &committed);
    let g2 = bulk(&mut vfs, 51, &committed);
    longer_records(&mut vfs, g2);
    let p = format!("{DIR}/{}", manifest_file_name(g2));
    let mut b = vfs.read_full(&p).unwrap();
    let k = b.len() - 1;
    b[k] ^= 0xff;
    put(&mut vfs, &manifest_file_name(g2), &b);
    let rep = cleanup_dir(&mut vfs, &mut NoWal, DIR, &committed).expect("a torn manifest is crash residue");
    assert_eq!(rep.effective_gen, g1);
}
