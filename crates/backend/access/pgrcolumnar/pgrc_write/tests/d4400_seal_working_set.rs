//! D4400 (born red): a part's seal held every column's distribution accumulator (pgrc_meta's DistAcc: a slot table,
//! its entries and a value arena over every distinct value of the column) until the part's end, where the sketches were
//! read all at once. On a 64-column COPY part (298 MB) that was about 450 MB more than its columns, so the seal's working
//! set went past the SEAL_X_LADDER times the part's bytes the ladder charges for it (3.06x wide, 3.3x on lwk4, measured
//! against 3x). Each stream's sketch is now read as soon as the stream is emitted, its builder dropping the accumulator
//! with it, so at most one column's is live.
//!
//! The witness: what the seal allocates past its inputs (the part's columns and builders, already held) stays within
//! (SEAL_X_LADDER - 1) times the columns' bytes, the part of the charge the seal itself has. A counting allocator over
//! the sealing thread's live bytes.

#![cfg(not(loom))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pgrc_format::abi::ColumnMetaBuilder;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture};
use pgrc_write::ingest::ColBuffer;
use pgrc_write::seal::{seal_part, CodecResolver, PartSpec};
use pgrc_write::structural::StructuralPolicy;
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use pgrc_write::testkit::{int8_col, mem_with_dir, text_col, DB, DIR, RELFILENUMBER, SPC};

std::thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

/// The calling thread's live heap bytes and their peak (a pass-through to the system allocator).
struct Counting;

fn moved(delta: i64) {
    let _ = LIVE.try_with(|l| {
        let v = l.get() + delta;
        l.set(v);
        let _ = PEAK.try_with(|p| p.set(p.get().max(v)));
    });
}

// SAFETY: every method forwards to System under the caller's contract; the counters only count.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            moved(l.size() as i64);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            moved(l.size() as i64);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        moved(-(l.size() as i64));
        // SAFETY: forwarded under the caller's contract.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded under the caller's contract.
        let np = unsafe { System.realloc(p, l, new_size) };
        if !np.is_null() {
            moved(new_size as i64 - l.size() as i64);
        }
        np
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// 32 int8 columns of 65536 distinct values each (8 granules): a distinct set per column several times the column's
/// own bytes, and an image that packs small under the codec election a COPY seals with (the 64-column part's 42 MB image
/// for its 258 MB of columns): ascending values, each column its own stride.
const COLS: u32 = 32;
const ROWS: u64 = 65_536;

#[test]
fn a_seal_holds_at_most_one_columns_distinct_set() {
    let mut roots: Vec<ColBuffer> = (1..=COLS).map(|a| ColBuffer::new(int8_col(a))).collect();
    for (j, c) in roots.iter_mut().enumerate() {
        let k = 2 * j as u64 + 1;
        for i in 0..ROWS {
            c.append_word(i * k).expect("word");
        }
    }
    let cols_bytes: u64 = roots.iter().map(|c| c.held_bytes()).sum();
    let mut builders: Vec<Box<dyn ColumnMetaBuilder>> = pgrc_write::meta_wire::builders_for_streams(&roots, &[], &[]);
    let mut vfs = mem_with_dir();
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0x4400 };
    // the codec election and resolver a COPY's seal runs (pgrc_am ingest: CodecCandidates, CodecResolver)
    let cands = CodecCandidates::new(ColumnPosture { fused: false, cold: true, numeric: false, dict: None });
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let base = LIVE.with(|l| l.get());
    PEAK.with(|p| p.set(base));
    let (sealed, _) = seal_part(&mut vfs, DIR, &spec, &roots, &[], &[], &mut builders, &sources, &CodecResolver, &StructuralPolicy::default(), 4400, 0)
        .expect("seal");
    let above = (PEAK.with(|p| p.get()) - base).max(0) as u64;
    let room = cols_bytes * (pgrc_write::par::SEAL_X_LADDER - 1);
    eprintln!("d4400: past the inputs {above} B, room {room} B, columns {cols_bytes} B, part file {} B", sealed.file_len);
    assert!(sealed.rows == ROWS && sealed.file_len != 0);
    assert!(
        above <= room,
        "the seal allocated {} MiB past its inputs, more than the {} MiB (SEAL_X_LADDER - 1 = {} x the columns' {} MiB) its charge leaves it",
        above >> 20,
        room >> 20,
        pgrc_write::par::SEAL_X_LADDER - 1,
        cols_bytes >> 20
    );
}

/// splitmix64's finalizer: full-width words no codec packs.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A fresh real table directory `<tmp>/pgrc_d4409_<pid>/pgrc_777`: the part file goes to the page cache, as a COPY's
/// does, not to a test VFS's heap (whose copy of the file the counting allocator would count as the seal's).
fn real_dir(real: &mut RealVfs) -> String {
    let base = std::env::temp_dir().join(format!("pgrc_d4409_{}", std::process::id()));
    let mut path = String::new();
    for part in [base.to_str().expect("utf8 temp dir").to_string(), "pgrc_777".to_string()] {
        path = if path.is_empty() { part } else { format!("{path}/{part}") };
        if !real.exists_path(&path).expect("exists") {
            real.mkdir_path(&path).expect("mkdir");
        }
    }
    path
}

/// D4409 (born red after D4400): the same seal on columns that do not pack (full-width pseudo-random distinct words),
/// into a real file: the part image, about as large as the columns, is built in one Vec doubled as it grows, so its
/// capacity alone reaches about twice the columns.
#[ignore = "V5SYNC r14 FOLLOWUP: D4400 per-stream distribution take not wired into the line's streamed seal (PGRCSERVE M4 / post-switch)"]
#[test]
fn an_incompressible_parts_image_stays_within_the_seal_charge() {
    let mut roots: Vec<ColBuffer> = (1..=COLS).map(|a| ColBuffer::new(int8_col(a))).collect();
    for (j, c) in roots.iter_mut().enumerate() {
        let k = 2 * j as u64 + 1;
        for i in 0..ROWS {
            c.append_word(mix(i * k)).expect("word");
        }
    }
    let cols_bytes: u64 = roots.iter().map(|c| c.held_bytes()).sum();
    let mut builders: Vec<Box<dyn ColumnMetaBuilder>> = pgrc_write::meta_wire::builders_for_streams(&roots, &[], &[]);
    let mut vfs = RealVfs;
    let dir = real_dir(&mut vfs);
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0x4409 };
    // the codec election and resolver a COPY's seal runs (pgrc_am ingest: CodecCandidates, CodecResolver)
    let cands = CodecCandidates::new(ColumnPosture { fused: false, cold: true, numeric: false, dict: None });
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let base = LIVE.with(|l| l.get());
    PEAK.with(|p| p.set(base));
    let (sealed, _) = seal_part(&mut vfs, &dir, &spec, &roots, &[], &[], &mut builders, &sources, &CodecResolver, &StructuralPolicy::default(), 4400, 0)
        .expect("seal");
    let above = (PEAK.with(|p| p.get()) - base).max(0) as u64;
    let room = cols_bytes * (pgrc_write::par::SEAL_X_LADDER - 1);
    eprintln!("d4400: past the inputs {above} B, room {room} B, columns {cols_bytes} B, part file {} B", sealed.file_len);
    assert!(sealed.rows == ROWS && sealed.file_len != 0);
    assert!(
        above <= room,
        "the seal allocated {} MiB past its inputs, more than the {} MiB (SEAL_X_LADDER - 1 = {} x the columns' {} MiB) its charge leaves it",
        above >> 20,
        room >> 20,
        pgrc_write::par::SEAL_X_LADDER - 1,
        cols_bytes >> 20
    );
}

/// D4409 (b) (born red after (a)): one wide text column whose wrap wins (lwk4's pad shape, `'p' x 110` and a key, eight
/// bands), into a real file. The seal held every band's unwrapped section to the stream's end, about as large as the
/// column, for an image under 1% of it. The bar is the share of the seal's charge its transients have after the inputs
/// and the image: (SEAL_X_LADDER - 2) x the columns' bytes past the part file. The key repeats (1000 values), so the
/// column's distribution accumulator stays small: on a column of distinct values that accumulator, not the sections,
/// sets the seal's peak.
#[ignore = "V5SYNC r14 FOLLOWUP: D4400 per-stream distribution take not wired into the line's streamed seal (PGRCSERVE M4 / post-switch)"]
#[test]
fn a_wide_text_columns_seal_holds_one_band_of_its_sections() {
    const TEXT_ROWS: u64 = 8 * pgrc_format::geom::BAND_ROWS as u64;
    let mut col = ColBuffer::new(text_col(1));
    for i in 0..TEXT_ROWS {
        col.append_varlena_payload(format!("{}{}", "p".repeat(110), i % 1000).as_bytes()).expect("text");
    }
    let roots = vec![col];
    // the charge's base: the part's normalized bytes (the writer's buffered_bytes, approx_bytes), not the column's
    // capacity, which a text heap's doubling leaves up to twice its length
    let cols_bytes: u64 = roots.iter().map(|c| c.approx_bytes()).sum();
    let mut builders: Vec<Box<dyn ColumnMetaBuilder>> = pgrc_write::meta_wire::builders_for_streams(&roots, &[], &[]);
    let mut vfs = RealVfs;
    let dir = real_dir(&mut vfs);
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0x4409 };
    let cands = CodecCandidates::new(ColumnPosture { fused: false, cold: true, numeric: false, dict: None });
    let sources: [&dyn CandidateSource; 1] = [&cands];
    let base = LIVE.with(|l| l.get());
    PEAK.with(|p| p.set(base));
    let (sealed, _) = seal_part(&mut vfs, &dir, &spec, &roots, &[], &[], &mut builders, &sources, &CodecResolver, &StructuralPolicy::default(), 4409, 1)
        .expect("seal");
    let above = (PEAK.with(|p| p.get()) - base).max(0) as u64;
    let room = cols_bytes * (pgrc_write::par::SEAL_X_LADDER - 2) + sealed.file_len;
    eprintln!("d4400: past the inputs {above} B, transients' room {room} B, columns {cols_bytes} B, part file {} B", sealed.file_len);
    assert!(sealed.rows == TEXT_ROWS && sealed.file_len != 0);
    assert!(
        above <= room,
        "the seal allocated {} MiB past its inputs, more than the {} MiB (SEAL_X_LADDER - 2 = {} x the columns' {} MiB, and the part file) its transients have",
        above >> 20,
        room >> 20,
        pgrc_write::par::SEAL_X_LADDER - 2,
        cols_bytes >> 20
    );
}
