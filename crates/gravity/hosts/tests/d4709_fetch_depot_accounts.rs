//! D4709 (Q24, the depot-fetch switch): a by-rowid fetch source parked in the slot depot with its worker set. Its kept
//! buffers and cursors leave the session that parked them for the depot's own rows (`Holder::SlotDepot` for the heap,
//! `Holder::Parked` for the part uses), as `Node::release_workers` moves a granule source's. On a REUSED thread, session A
//! parks the set and ends, and session B on the same thread takes it (`Node::provision`'s unpark + `transfer_held`), runs a
//! statement over it, parks it again and drops it (an eviction). A's session ends holding nothing of the set, no session
//! or statement ends with a residue, and the depot's rows come back to where they started. One test: the ledger's rows
//! are process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source};
use gravity_hosts::ops::scan::pgrc::fetch_site::{FetchRecipe, FetchSource, KIND_INT};
use gravity_hosts::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec, Resolved};
use memledger::charge::owner_row;
use memledger::owner_end::{totals, Source as EndSource};
use memledger::{charge, Acct, Holder, Owner, OwnerClass, Release};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::MemPartIo;
use pgrc_read::{PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const REL: u32 = 4709;
const ROWS: u64 = 60_000;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

fn fixture(reg: &Arc<PartRegistry>) -> (Vec<pgrc_read::registry::PartPin>, Arc<PartList>) {
    let bf = build_fixture(int8_fixture("gravity_d4709_fetch_depot", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some(i as i64 * 7)));
    let (mut pins, mut specs) = (Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = 4709 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        let part = Arc::clone(pin.part());
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(part) });
        pins.push(pin);
    }
    (pins, Arc::new(PartList::new(specs)))
}

/// One statement on the calling thread's backend: bind, fetch a row of every part, unbind. The rows' values come back.
fn statement(src: &mut FetchSource, list: &Arc<PartList>) {
    memledger::statement_begin();
    let resolved = Resolved::for_rel(REL, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut base = 0u64;
    let mut rowids = Vec::new();
    let mut want = Vec::new();
    for (p, spec) in list.parts().iter().enumerate() {
        let local = spec.rows - 1;
        rowids.push(((p as u64) << 32) | ((local / 8192) << 13) | (local % 8192));
        want.push((base + local) as i64 * 7);
        base += spec.rows;
    }
    let mut lanes = [Lane::NONE; 4];
    assert_eq!(src.fetch(&rowids, &mut lanes).unwrap() as usize, rowids.len(), "a row of every part");
    for (j, w) in want.iter().enumerate() {
        assert_eq!(unsafe { lanes[0].datum(j) } as i64, *w, "row {j}'s value");
    }
    src.unbind();
    drop(resolved);
    memledger::statement_end();
}

/// The depot's park of the source, as `Node::release_workers` makes it (keep-warm off).
fn park(src: &mut FetchSource) {
    src.park();
    src.kept_uses_to(Owner::Cache(Holder::Parked), Release::Evict);
    src.transfer_held(Owner::Cache(Holder::SlotDepot), Release::Evict);
}

/// The depot's take, as `Node::provision` makes it after `DEPOT.take`: to the taking backend's context.
fn take(src: &mut FetchSource) {
    src.unpark();
    let (to, release) = charge::context();
    src.transfer_held(to, release);
}

fn residues() -> [memledger::owner_end::Totals; 3] {
    [totals(EndSource::Owner, OwnerClass::Session), totals(EndSource::Owner, OwnerClass::Statement), totals(EndSource::Owner, OwnerClass::Portal)]
}

#[test]
fn a_parked_fetch_set_is_the_depots_and_never_its_ended_sessions() {
    std::thread::spawn(|| {
        let reg = Arc::new(PartRegistry::new(64 << 20));
        let (_pins, list) = fixture(&reg);
        assert!(list.parts().len() >= 3, "the fixture cut into parts");
        let recipe = FetchRecipe::for_test(REL, &[1], &[KIND_INT], Arc::clone(&reg));
        let mut src = FetchSource::new(recipe);
        let (depot, parked) = (Owner::Cache(Holder::SlotDepot), Owner::Cache(Holder::Parked));
        let (d0, p0) = (owner_row(depot), owner_row(parked));
        let r0 = residues();

        // session A runs a statement: unbound, the source's kept buffers and cursors are A's session's
        memledger::bind_backend(470, 94_709);
        let sa = Owner::Session(Acct::backend(470));
        let a0 = owner_row(sa);
        statement(&mut src, &list);
        let a1 = owner_row(sa);
        assert!(a1.committed > a0.committed, "A's session holds the kept buffers ({} -> {} B)", a0.committed, a1.committed);
        assert!(a1.uses > a0.uses, "and the kept cursors' part uses ({} -> {})", a0.uses, a1.uses);

        // A's statement end parks the set: every byte and use leaves A for the depot's rows
        park(&mut src);
        let a2 = owner_row(sa);
        assert_eq!((a2.committed, a2.uses), (a0.committed, a0.uses), "parked: nothing of the set is A's");
        let (held, uses) = (a1.committed - a0.committed, a1.uses - a0.uses);
        assert_eq!(owner_row(depot).committed - d0.committed, held, "the depot's holder has the kept heap");
        assert_eq!(owner_row(parked).uses - p0.uses, uses, "the parked holder has the part uses");

        // A ends on this thread; B binds the same thread next (a reused thread) and takes the set
        memledger::unbind_backend();
        assert_eq!(residues(), r0, "A ended with no residue");
        memledger::bind_backend(471, 94_710);
        let sb = Owner::Session(Acct::backend(471));
        let b0 = owner_row(sb);
        take(&mut src);
        assert_eq!(owner_row(sb).committed - b0.committed, held, "the take: the kept heap is B's");
        assert_eq!(owner_row(depot).committed, d0.committed, "and no longer the depot's");

        // B's statement over the taken set reads the same rows, then B parks it again and an eviction drops it
        statement(&mut src, &list);
        park(&mut src);
        let b1 = owner_row(sb);
        assert_eq!((b1.committed, b1.uses), (b0.committed, b0.uses), "parked again: nothing of the set is B's");
        drop(src);
        let (d1, p1) = (owner_row(depot), owner_row(parked));
        assert_eq!((d1.committed, p1.uses), (d0.committed, p0.uses), "the eviction gave the depot's rows back to their start");
        assert_eq!((owner_row(sb).committed, owner_row(sb).uses), (b0.committed, b0.uses), "and charged B nothing");
        memledger::unbind_backend();
        assert_eq!(residues(), r0, "no session, statement or portal ended with a residue");
    })
    .join()
    .unwrap();
}
