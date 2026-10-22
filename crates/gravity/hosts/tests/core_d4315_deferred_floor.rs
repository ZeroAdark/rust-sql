//! D4315: a part a running statement deferred is not in the part cache's floor. D3823's gate keeps a part every claim of
//! the statement left (deferred) while the cache has room, for the statement's next run. On trunk such a part stayed in
//! the cache's floor (`PartRegistry::in_use_bytes`: what no trim may evict, the memory broker's floor for the cache), and
//! under the broker the cache's budget, the level the gate's release waits on, never went below that floor: the deferred
//! parts raised their own release level and went back only at the statement's end (the watchdog's victims held 455-544 MB
//! of such parts, charged to no one). Now the floor counts only parts some live claim holds: a deferred part is
//! reclaimable; entered again (a later pass) it is back in the floor. A process of its own: the cache's budget and B's
//! snapshot are process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::MemPartIo;
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const ROWS: u64 = 60_000;
const REL: u32 = 4515;
const G: u64 = 1 << 30;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

/// Three parts of a 60,000-row int8 fixture opened through `reg` (so its floor walks them), and their list.
fn fixture(reg: &Arc<PartRegistry>) -> (Vec<Arc<OpenPart>>, Vec<pgrc_read::registry::PartPin>, Arc<PartList>) {
    let bf = build_fixture(int8_fixture("gravity_d4315_floor", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let (mut parts, mut pins, mut specs) = (Vec::new(), Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = 1010 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        let part = Arc::clone(pin.part());
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(part.clone()) });
        parts.push(part);
        pins.push(pin);
    }
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    (parts, pins, Arc::new(PartList::new(specs)))
}

/// One claim of granules `[g, g + 1)` read whole.
fn claim(src: &mut GranuleRangeSource, g: u64) -> u64 {
    let mut lanes = [Lane::NONE; MAX_LANES];
    src.seek(&[g, g + 1], &[0, 0]).unwrap();
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return rows;
        }
        rows += n as u64;
    }
}

fn cached(parts: &[Arc<OpenPart>]) -> u64 {
    parts.iter().map(|p| p.cache_bytes()).sum()
}

#[test]
fn a_deferred_part_is_not_in_the_part_caches_floor() {
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let (parts, _pins, list) = fixture(&reg);
    // the part cache far inside its budget and the box with room (B's free bytes 12 GiB): D3823's gate defers every part
    pgrc_read::partcache::set_budget(1 << 40);
    memledger::set_fallback_limit(16 * G);
    memledger::note_snapshot(0, 12 * G);
    memledger::bind_backend(196, 9196);

    memledger::statement_begin();
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::clone(&reg));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(None);
    resolved.install(REL, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let rows: u64 = (0..list.granules()).map(|g| claim(&mut src, g)).sum();
    assert_eq!(rows, ROWS);
    assert_eq!(resolved.part_release_counts().0, 0, "with room the gate keeps every finished part for the statement's next run (D3268)");
    assert!(parts.iter().all(|p| p.live_users() >= 1), "the running statement still uses every part");
    assert!(cached(&parts) > 0, "the parts' streams are cached");
    assert_eq!(reg.in_use_bytes(), 0, "every claim left every part: the deferred parts are not in the cache's floor (trunk: all of them, {} bytes)", cached(&parts));

    // a later pass enters the first part again: a live claim reads it, so it is in the floor until its claims leave it
    let mut lanes = [Lane::NONE; MAX_LANES];
    src.seek(&[0, 1], &[0, 0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    assert!(src.window(&mut w).unwrap() > 0);
    assert_eq!(reg.in_use_bytes(), parts[0].cache_bytes(), "entered again: the first part is in a live claim's read, the others stay out");

    src.unbind();
    drop(resolved);
    memledger::statement_end();
    assert_eq!(reg.in_use_bytes(), 0, "the statement's end");
    drop(src.release_rel(0).expect("every relation's kept cursors"));
}
