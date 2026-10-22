//! NEW-53 (D1443): a resident keyed end (a hashed aggregate whose records keep key copies, `Spill::None`) hands a continuing
//! arm's phase-1 tables to its run's last arm through the slot's stream, as the word form scatters them into spill files:
//! into a memory file while the statement's grant holds the records (`SpillTable::hand_own`, `HAND_MEMORY`), else into a
//! disk file (`HAND_DISK`). The memory file carries every record, partition by partition, byte for byte as the disk
//! scatter writes them, at the table's partition count (the owner phase's `spill_bits`), and stays out of the spill census.
use gravity_driver_rig as common;

use core::sync::atomic::Ordering;

use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, T1, t1_default_bytes};
use gravity_abi::hash::KeyHash;
use gravity_driver::mem::spill::{SpillTable, HANDED, HAND_DISK, HAND_MEMORY};
use gravity_driver::{RunReader, SpillFile};

const PBITS: u8 = 4;
const REGION: usize = 4 << 20;

/// A T1 laid in `region` holding `n` buffered records (keys `(i, i * 7)`), and its record bytes.
fn filled(region: &mut [u64], layout: &RecordLayout, n: u64) -> (*mut u8, usize) {
    let p = region.as_mut_ptr() as *mut u8;
    // SAFETY: `region` is REGION zeroed, 8-aligned bytes; `at` lays the table over its zero header.
    let t = unsafe { T1::at(p, layout, PBITS, t1_default_bytes(), REGION as u32) } as *mut T1;
    for i in 0..n {
        let keys = [i, i * 7];
        // SAFETY: `t` is the live table laid above
        unsafe {
            let part = (*t).part_of(&keys, KeyHash::of(&keys));
            T1::append_new(t, part, &keys).expect("the pool holds the records");
        }
    }
    // SAFETY: as above
    let held = unsafe { (*t).held() };
    assert_eq!(held, n, "every record buffered");
    (p, held as usize * layout.size() as usize)
}

/// Partition `part`'s bytes of `f`, read whole (`read_part`) and through its cursor (`open_part`), which agree.
fn part_bytes(f: &dyn SpillFile, part: u32) -> Vec<u8> {
    let mut whole = Vec::new();
    f.read_part(part, &mut |b| {
        whole.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let mut pulled = Vec::new();
    if let Some(mut r) = f.open_part(part).unwrap() {
        let mut buf = [0u8; 333];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            pulled.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(whole, pulled, "part {part}: the cursor reads the part's bytes");
    // a memory file counts each part's bytes (the owner sizes from them); the test's disk stand-in says 0 (`part_bytes`' default)
    if f.in_memory() {
        assert_eq!(f.part_bytes(part), whole.len() as u64, "part {part}: its byte count");
    }
    whole
}

#[test]
fn a_resident_hand_off_carries_every_record_in_memory_out_of_the_census() {
    gravity_driver::install_spill_factory(&common::MEM_SPILL);
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    let mut spill = SpillTable::with_slots(2);
    spill.begin_life(0);
    spill.begin_life(1);
    let (mut a, mut b, mut c) = (vec![0u64; REGION / 8], vec![0u64; REGION / 8], vec![0u64; REGION / 8]);
    let ((pa, bytes), (pb, _)) = (filled(&mut a, &layout, 5000), filled(&mut b, &layout, 5000));
    let before = HANDED[HAND_MEMORY].load(Ordering::Relaxed);
    // SAFETY: `pa` / `pb` are live tables of `bytes` record bytes (`filled`)
    let moved = unsafe { spill.hand_own(0, pa, bytes, true) }.unwrap();
    assert_eq!(moved, 5000);
    assert!(HANDED[HAND_MEMORY].load(Ordering::Relaxed) >= before + 5000, "the census of handed records");
    // the word form's scatter of the same records into a disk file of slot 1
    unsafe { spill.scatter_own(1, 0, pb, 0) }.unwrap();
    let (mem, disk) = (spill.stream(0), spill.stream(1));
    assert!(mem.handed() && !mem.spilled(), "a hand-off in memory is no spill");
    assert_eq!(mem.spilled_bytes(), 0, "nor any spilled byte");
    assert_eq!(mem.gen0_parts(), 0, "nor parts a refill would claim");
    assert_eq!(mem.bits0(), PBITS, "the owner phase reads it at the table's partition count");
    assert!(disk.spilled() && !disk.handed());
    let (mf, df) = (mem.files[HAND_MEMORY].as_deref().expect("the memory file"), disk.files[0].as_deref().expect("the disk file"));
    assert!(mf.in_memory() && !df.in_memory());
    assert_eq!(mf.spilled_bytes(), bytes as u64, "the owner sizes from its record bytes");
    for part in 0..1u32 << PBITS {
        assert_eq!(part_bytes(mf, part), part_bytes(df, part), "part {part}: the records the word form's scatter writes");
    }
    // a second arm's table joins the same memory file, at the same partition count
    let (pc, bytes_c) = filled(&mut c, &layout, 300);
    unsafe { spill.hand_own(0, pc, bytes_c, true) }.unwrap();
    let mf = spill.stream(0).files[HAND_MEMORY].as_deref().unwrap();
    assert_eq!(mf.spilled_bytes(), (bytes + bytes_c) as u64);
    assert_eq!((0..1u32 << PBITS).map(|p| part_bytes(mf, p).len()).sum::<usize>(), bytes + bytes_c);
    // the life's end drops the file and counts nothing spilled for it
    spill.end_life(0);
    assert!(!spill.stream(0).handed());
    spill.end_life(1);
    assert!(spill.spilled(), "slot 1's life spilled");
    assert_eq!(spill.spilled_bytes(), bytes as u64, "slot 1's bytes alone");
}

#[test]
fn a_refused_hand_off_goes_to_a_disk_file() {
    gravity_driver::install_spill_factory(&common::MEM_SPILL);
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    let mut spill = SpillTable::with_slots(1);
    spill.begin_life(0);
    let (mut a, mut b) = (vec![0u64; REGION / 8], vec![0u64; REGION / 8]);
    let ((pa, bytes_a), (pb, bytes_b)) = (filled(&mut a, &layout, 2000), filled(&mut b, &layout, 700));
    let before = HANDED[HAND_DISK].load(Ordering::Relaxed);
    // the first table in memory, the next one refused (the grant's room): both read by the owner phase, one partition count
    unsafe { spill.hand_own(0, pa, bytes_a, true) }.unwrap();
    unsafe { spill.hand_own(0, pb, bytes_b, false) }.unwrap();
    assert!(HANDED[HAND_DISK].load(Ordering::Relaxed) >= before + 700);
    let s = spill.stream(0);
    assert!(s.handed() && s.spilled(), "a refused hand-off spills");
    assert_eq!(s.spilled_bytes(), bytes_b as u64, "its disk bytes alone");
    assert_eq!(s.bits0(), PBITS);
    let files: Vec<&dyn SpillFile> = s.files.iter().flatten().map(|f| f.as_ref()).collect();
    assert_eq!(files.len(), 2);
    assert!(files[HAND_MEMORY].in_memory() && !files[HAND_DISK].in_memory());
    let total: usize = files.iter().map(|f| (0..1u32 << PBITS).map(|p| part_bytes(*f, p).len()).sum::<usize>()).sum();
    assert_eq!(total, bytes_a + bytes_b, "every record of both tables");
}

/// CONVERSION WAVE 2 (R4 b, c): a memory hand-off's chunk is charged to the slot's owner as the hand-off reserves it, with
/// its shared block and the file's chunk list; a reader shares the chunk in place (its extents list alone is charged, no
/// copy of its partition), keeps it and its charge past the stream's life end, and its drop on another thread (a worker's
/// close) gives the charge back there: the owner ends at 0.
#[test]
fn wave2_a_memory_hand_off_is_charged_to_the_slot_owner_and_its_reader_shares_the_chunk() {
    const PROC: u32 = 4072;
    gravity_driver::install_spill_factory(&common::MEM_SPILL);
    let (bytes, charged, reading, after_life, read_back, after_reader, want_len) = std::thread::spawn(|| {
        memledger::bind_backend(PROC, 9000 + PROC);
        memledger::statement_begin();
        let stmt = memledger::charge::context().0;
        let o = memledger::charge::op_begin(stmt, memledger::Kind::HashAgg as u32, 0);
        let held = || memledger::charge::usage(o).held;
        let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
        let mut spill = SpillTable::with_slots(1);
        spill.begin_life(0);
        spill.set_owner(0, Some(o));
        let mut a = vec![0u64; REGION / 8];
        let (pa, bytes) = filled(&mut a, &layout, 5000);
        // SAFETY: `pa` is a live table of `bytes` record bytes (`filled`)
        unsafe { spill.hand_own(0, pa, bytes, true) }.unwrap();
        let charged = held();
        let mf = spill.stream(0).files[HAND_MEMORY].as_deref().expect("the memory file");
        let part = (0..1u32 << PBITS).find(|&p| mf.part_bytes(p) > 0).expect("a partition with records");
        let want = part_bytes(mf, part);
        let r = mf.open_part(part).unwrap().expect("a reader");
        let reading = held();
        spill.end_life(0);
        let after_life = held();
        let got = std::thread::spawn(move || {
            let mut r = r;
            let (mut got, mut buf) = (Vec::new(), [0u8; 1000]);
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            got
        })
        .join()
        .unwrap();
        let after_reader = held();
        memledger::charge::op_end(o);
        let _ = memledger::statement_end();
        memledger::unbind_backend();
        (bytes as i64, charged, reading, after_life, got == want, after_reader, want.len() as i64)
    })
    .join()
    .unwrap();
    assert!(charged > bytes, "the hand-off's chunk, its shared block and the chunk list are the slot owner's: {charged} <= {bytes}");
    let (list, part) = (reading - charged, want_len);
    assert!(list > 0 && list < part / 8, "a reader holds its extents list alone, on the owner, never a copy of its partition: {list} bytes of {part}");
    assert!(after_life >= bytes + list && after_life <= reading, "the reader keeps the chunk and its list, and their charge, past the life's end: {after_life}");
    assert!(read_back, "the reader reads its partition's bytes after the file is gone");
    assert_eq!(after_reader, 0, "the reader's drop on another thread gives the charge back");
}
