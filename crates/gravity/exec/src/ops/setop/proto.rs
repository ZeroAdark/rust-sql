//! M9's runtime protocols for v4 programs (the merged M9's set-operation part, V4R-388): `setop.setop` in three forms, all
//! declared by lowerv4's setop/ files. Created by SETOP (owner M9-HOSTS).
//! - The copy form (one word; setop/setop.rs): after both groups' producers ran (the right one may run first, CAP-36),
//!   every left group record gets PG's output count (nodeSetOp.c set_output_count) in its copy word from its own count
//!   and the right group's count of the same key; the copy word's byte offset + 1 goes in the step's word at prepare.
//! - The plain form (one word; F-ZERO, no column): each side is a plain group whose one record counts its rows; after
//!   both sides ran, PG's output count of the two counts goes behind the step's word (a pointer to it).
//! - The skip form (no word; setop/empty_left.rs, R9-L09): after the left group's producer ran, an empty left runs the
//!   right side's pipelines as if no claim reached them, as PG never reads the inner plan then. Those pipelines come
//!   from the program (`skip_mask`), never from `arg` (D1274), so a program a pass reorders keeps them. m9_skip.rs.
//! - `setop.recursive`'s generation object (W8-RECURSE; one handle word): a term error kept until the rows before it are
//!   returned. m9_rows.rs.

use core::cell::Cell;

use gravity_abi::breaker::{RecordLayout, SlotKind};
use gravity_abi::hash::{Depth, KeyHash};
use gravity_driver::Exec;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Program;
use types_error::PgResult;

use gravity_fn::arena::{AVec, Kind, OpArena, Refused};

use super::raw::{key_len, owned_each, Record};
use crate::ops::{Live, Prepare, Protocol};
use crate::admit::{Admit, Bind};
use crate::glue;

pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];

pub const PROTOCOLS: &[&dyn Protocol] = &[&SetOp, &crate::ops::setop::generation::Gen];

/// lowerv4 setop/setop.rs's `arg` fields (the crates do not depend on each other; the lowerv4 tests pin both). No field
/// read here names a pipeline or a slot (D1274, D2197): both come from the program (`consumes_from`, `right_slot`).
const ARG_CMD_MASK: u64 = 0x0f;
const ARG_NKEYS_SHIFT: u32 = 32;
const ARG_NKEYS_MASK: u64 = 0xffff;
/// D1168: the key image's words (lowerv4 `image_words`), the first NULL flags word's index in a record; bit 63 above them
/// is the kernels' `SETOP_ARG_LOUD`.
const ARG_WORDS_SHIFT: u32 = 48;
const ARG_WORDS_MASK: u64 = 0x7fff;
const ARG_PLAIN: u64 = 1 << 4;
const ARG_PLAIN_LEFT_SHIFT: u32 = 32;
const ARG_PLAIN_RIGHT_SHIFT: u32 = 48;
/// D2805: a keyed setop.setop proto (it holds its word, ARG_PLAIN clear) that lowerv4 marked with the kernels'
/// `SETOP_ARG_LOUD` bit: a root-path sorted set operation whose left streams a raise (runner/admission.rs).
pub(crate) fn counted_pull_mark(p: &Proto) -> bool {
    p.kind == ProtoKind::SetOp && p.words.n != 0 && p.arg & ARG_PLAIN == 0 && p.arg & gravity_kernels::ops::setop::entries::SETOP_ARG_LOUD != 0
}
pub(crate) use gravity_abi::breaker::MIN_REGION;

pub(crate) struct SetOp;

struct CopiesLive {
    left: u8,
    right: u8,
    /// The later of the two sides' producers (CAP-36: decompose maps the larger side first, so the right group's
    /// producer can run before the left's); the counts are filed once both ran.
    at: usize,
    cmd: u64,
    nkeys: usize,
    /// The key image's words (D1168: a key is 1 or 2 words; the NULL flags words, one per 64 keys, follow them).
    words: usize,
    llay: RecordLayout,
    rlay: RecordLayout,
    /// The left record's copy slot (the `Count` slot that is not the sentinel).
    copy: usize,
}

/// F-ZERO: the plain form. The step's word points at `count`, filed once the right side ran.
struct PlainLive {
    left: u8,
    right: u8,
    /// `CopiesLive::at`.
    at: usize,
    cmd: u64,
    lsent: usize,
    rsent: usize,
    count: Box<Cell<i64>>,
}

/// D2197: the right slot as `p`'s peer makes it (lowerv4's renumber_deferred never renames `arg`'s), if its emit reads it.
fn right_slot(p: &Proto, program: &Program) -> Option<u8> {
    program.pipelines.get(p.peer as usize)?.produces().filter(|s| program.pipelines.get(p.pipeline as usize).is_some_and(|e| e.consumes.contains(s)))
}

fn owner_layout(cx: &Prepare<'_>, p: usize) -> PgResult<RecordLayout> {
    match cx.program.pipelines.get(p).and_then(|q| q.owner.as_ref()) {
        Some(o) => Ok(o.layout),
        None => Err(glue::internal("gravity: setop.setop over a side without its owner layout")),
    }
}

impl Protocol for SetOp {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SetOp
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let (k, peer) = (p.pipeline as usize, p.peer as usize);
        if p.words.n == 0 {
            // the skip form rides the left producer (`pipeline`) and names the right producer (`peer`); the emit is the
            // pipeline that reads the left slot from it
            return Ok(crate::ops::setop::skip::prepare(p, cx.program, cx.charged_vec()));
        }
        let Some(right) = right_slot(p, cx.program) else { return Err(glue::internal("gravity: setop.setop's emit does not read its right producer's slot")) };
        let base = p.words.base as usize;
        if p.arg & ARG_PLAIN != 0 {
            let count = Box::new(Cell::new(0i64)); // alloc: Exec
            match (p.words.n, cx.words.get_mut(base), cx.nulls.get_mut(base)) {
                (1, Some(w), Some(n)) => (*w, *n) = (count.as_ptr() as u64, 0),
                _ => return Err(glue::internal("gravity: setop.setop without its word")),
            }
            let (lsent, rsent) = (((p.arg >> ARG_PLAIN_LEFT_SHIFT) & 0xffff) as usize, ((p.arg >> ARG_PLAIN_RIGHT_SHIFT) & 0xffff) as usize);
            let at = crate::ops::setop::skip::producer_of(cx.program, k, p.slot).map_or(peer, |lp| lp.max(peer));
            return Ok(Some(Box::new(PlainLive { left: p.slot, right, at, cmd: p.arg & ARG_CMD_MASK, lsent, rsent, count }))); // alloc: Exec
        }
        let Some(lp) = crate::ops::setop::skip::producer_of(cx.program, k, p.slot) else {
            return Err(glue::internal("gravity: setop.setop's emit does not read its left group"));
        };
        let (llay, rlay) = (owner_layout(cx, lp)?, owner_layout(cx, peer)?);
        let s = llay.sentinel_slot();
        let Some(copy) = (0..llay.nslots()).find(|&j| j != s && llay.slot_kind(j) == SlotKind::Count) else {
            return Err(glue::internal("gravity: setop.setop's left record without its copy slot"));
        };
        match (p.words.n, cx.words.get_mut(base), cx.nulls.get_mut(base)) {
            (1, Some(w), Some(n)) => (*w, *n) = (llay.slot_off(copy) as u64 + 1, 0),
            _ => return Err(glue::internal("gravity: setop.setop without its word")),
        }
        let (nkeys, words) = (((p.arg >> ARG_NKEYS_SHIFT) & ARG_NKEYS_MASK) as usize, ((p.arg >> ARG_WORDS_SHIFT) & ARG_WORDS_MASK) as usize);
        Ok(Some(Box::new(CopiesLive { left: p.slot, right, at: lp.max(peer), cmd: p.arg & ARG_CMD_MASK, nkeys, words, llay, rlay, copy }))) // alloc: Exec
    }
}

/// A plain side's row count: its one record's sentinel at byte `sent`, 0 when no record was laid out (raw.rs
/// `region_count`).
pub(crate) fn plain_count(exec: &mut Exec, slot: u8, sent: usize) -> Option<i64> {
    super::raw::region_count(exec.node_mut().slot_region(slot), sent)
}

/// Every record of slot `slot`'s owned region handed to `f` (`agg_gather`'s runs-form walk, raw.rs `owned_each`, no list
/// made): their number, or `None` when the region is not an owned table; 0 when it was never laid out.
fn each_record(exec: &mut Exec, slot: u8, lay: &RecordLayout, f: impl FnMut(Record)) -> Option<usize> {
    let r = exec.node_mut().slot_region(slot);
    if r.len() < MIN_REGION {
        return Some(0);
    }
    owned_each(r, lay.size() as usize, f)
}

/// PG 18's output count (nodeSetOp.c set_output_count) of a group counted `l` on the left and `r` on the right.
fn copies(cmd: u64, l: i64, r: i64) -> i64 {
    match cmd {
        0 => (l > 0 && r > 0) as i64,
        1 => l.min(r).max(0),
        2 => (l > 0 && r == 0) as i64,
        _ => (l - r).max(0),
    }
}

impl Live for CopiesLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.at {
            return None;
        }
        // a set side never spills (decompose agg.rs `set_side`: `Spill::None`, its region grows past its share, D254), so
        // its owned table holds every group, the same records keyed.emit_group's `agg_gather` walks. The slot's spill
        // census is not a test: the owner phase's in-memory runs count there too. A region that is not an owned table is
        // the unreachable case.
        // both sides counted first (the directories only): either one not an owned table is the unreachable case
        let (Some(_), Some(nr)) = (each_record(exec, self.left, &self.llay, |_| ()), each_record(exec, self.right, &self.rlay, |_| ())) else {
            return Some(Err(gravity_driver::Error::host("gravity: bug: setop.setop over a side that is not an owned table")));
        };
        let (sl, sr, n, w) = (self.llay.sentinel_slot(), self.rlay.sentinel_slot(), self.nkeys, self.words);
        if self.llay.nkeys() < w || self.rlay.nkeys() < w {
            return Some(Err(gravity_driver::Error::host("gravity: setop.setop's sides are narrower than their keys")));
        }
        // CONVERT (wave 2): the right side's count index is that side's working memory: carved from a scratch arena on the
        // right slot's operator owner (`Node::slot_owner`, its HashAgg kind: a keyed set side is a hashed or sorted group,
        // decompose agg.rs `set_side`), the allocator's chunks charged to that operator's row; a carve the allocator refuses
        // is the statement's out of memory (53200), not the process's abort
        let a = OpArena::scratch_of(exec.node().slot_owner(self.right), Kind::HashAgg);
        let mut right = match Counts::with_capacity(&a, nr, key_len(n, w)) {
            Ok(c) => c,
            Err(r) => return Some(Err(refused(r))),
        };
        let mut added = Ok(());
        each_record(exec, self.right, &self.rlay, |r| {
            if added.is_ok() {
                // a record of the right layout, its sentinel an i64 count
                r.key_into(&self.rlay, n, w, right.probe());
                added = right.add(r.count(self.rlay.slot_off(sr) as usize));
            }
        });
        if let Err(r) = added {
            return Some(Err(refused(r)));
        }
        each_record(exec, self.left, &self.llay, |l| {
            // a record of the left layout: its sentinel and copy slot are i64 counts; only this protocol writes the copy
            // slot, before the emit that reads it runs
            let c = l.count(self.llay.slot_off(sl) as usize);
            l.key_into(&self.llay, n, w, right.probe());
            let r = right.get();
            l.set_count(self.llay.slot_off(self.copy) as usize, copies(self.cmd, c, r));
        });
        Some(Ok(()))
    }
}

/// The right side's count per key: its keys in one flat buffer (`len` words each), an open-addressed index over them
/// homed by the one key hash, and a probe key written in place, so a record costs no allocation. CONVERT (wave 2): every
/// buffer is carved whole, once, from the caller's arena `a` (its owner the right slot's operator), sized by the right
/// side's records: no growth.
struct Counts<'a> {
    a: &'a OpArena,
    len: usize,
    bits: u32,
    /// Key index + 1 per slot, 0 when empty.
    slots: AVec<u32>,
    keys: AVec<u64>,
    counts: AVec<i64>,
    probe: AVec<u64>,
}

impl<'a> Counts<'a> {
    /// The index for at most `n` keys of `len` words in `a`; a carve the allocator refuses is `Refused`.
    fn with_capacity(a: &'a OpArena, n: usize, len: usize) -> Result<Counts<'a>, Refused> {
        let bits = (2 * n.max(8)).next_power_of_two().trailing_zeros();
        let mut slots = AVec::with_capacity(a, 1 << bits)?;
        slots.resize(a, 1 << bits, 0)?;
        let mut probe = AVec::with_capacity(a, len)?;
        probe.resize(a, len, 0)?;
        let (keys, counts) = (AVec::with_capacity(a, n * len)?, AVec::with_capacity(a, n)?);
        Ok(Counts { a, len, bits, slots, keys, counts, probe })
    }

    /// The probe key's words, for the caller to write.
    fn probe(&mut self) -> &mut [u64] {
        self.probe.as_mut_slice(self.a)
    }

    /// The probe key's slot: the one holding it, else the empty one where it goes.
    fn slot(&self) -> usize {
        let (slots, keys, probe) = (self.slots.as_slice(self.a), self.keys.as_slice(self.a), self.probe.as_slice(self.a));
        let mask = slots.len() - 1;
        let mut i = KeyHash::of(probe).home(self.bits, Depth::TOP);
        loop {
            match slots[i] as usize {
                0 => return i,
                j if keys[(j - 1) * self.len..j * self.len] == probe[..] => return i,
                _ => i = (i + 1) & mask,
            }
        }
    }

    /// `c` added to the probe key's count (a key the index lacks enters it at `c`). The index holds at most half its
    /// slots' worth of keys (`with_capacity`'s), so a probe always ends at an empty slot; a new key stays within
    /// `with_capacity`'s room (one per right record), so it carves nothing.
    fn add(&mut self, c: i64) -> Result<(), Refused> {
        let (i, a) = (self.slot(), self.a);
        match self.slots.as_slice(a)[i] as usize {
            0 => {
                self.keys.extend_from_slice(a, self.probe.as_slice(a))?;
                self.counts.push(a, c)?;
                self.slots.as_mut_slice(a)[i] = self.counts.len() as u32;
            }
            j => self.counts.as_mut_slice(a)[j - 1] += c,
        }
        Ok(())
    }

    /// The probe key's count, 0 when the right side has no such key.
    fn get(&self) -> i64 {
        match self.slots.as_slice(self.a)[self.slot()] as usize {
            0 => 0,
            j => self.counts.as_slice(self.a)[j - 1],
        }
    }
}

/// CONVERT (wave 2): a scratch carve the allocator refused: the statement's out of memory, as a refused region's.
fn refused(r: Refused) -> gravity_driver::Error {
    gravity_driver::Error::out_of_memory(r.bytes)
}

impl Live for PlainLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.at {
            return None;
        }
        let (Some(l), Some(r)) = (plain_count(exec, self.left, self.lsent), plain_count(exec, self.right, self.rsent)) else {
            return Some(Err(gravity_driver::Error::host("gravity: bug: setop.setop over a plain side that is not a record table")));
        };
        self.count.set(copies(self.cmd, l, r));
        Some(Ok(()))
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::breaker::{HashAgg, RecordLayout};

    use gravity_abi::breaker::{KeyKind, SlotKind};

    use super::copies;
    use crate::ops::setop::raw::{key, region_count};

    /// D1168: keys of one and two words (a text's Fp128, an int, a uuid) with the NULL flags word after all five image
    /// words, where keyed/hashagg_rec.rs `flags_word` puts it (the sum of the keys' words), not after the three keys. A
    /// NULL in each position meets only its own NULL; two texts differing only in the second Fp128 word differ.
    #[test]
    fn a_set_key_reads_its_flags_past_every_image_word() {
        let l = RecordLayout::new(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64, KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64], &[SlotKind::Count]);
        let (n, w) = (3, 5);
        let rec = |words: [u64; 5], flags: u64| {
            let mut r = vec![0u64; (l.size() as usize).div_ceil(8)]; // alloc: Cold
            for (i, v) in words.iter().chain([flags].iter()).enumerate() {
                r[l.key_off(i) / 8] = *v;
            }
            r
        };
        // SAFETY: each record is a zeroed buffer of `l`'s size holding its six key words
        let k = |r: &Vec<u64>| unsafe { key(r.as_ptr() as *const u8, &l, n, w) };
        let full = rec([1, 2, 7, 3, 4], 0);
        assert_eq!(k(&full), vec![1, 2, 7, 3, 4, 0], "five image words, then the flags"); // alloc: Cold
        assert_ne!(k(&full), k(&rec([1, 9, 7, 3, 4], 0)), "a text differing only past its first word");
        // a NULL key's words are zero (emit_image); its flag bit i is key i's, read from word 5, not word 3
        let nulls = [rec([0, 0, 7, 3, 4], 1), rec([1, 2, 0, 3, 4], 2), rec([1, 2, 7, 0, 0], 4)];
        for (i, a) in nulls.iter().enumerate() {
            assert_eq!(k(a)[w], 1 << i, "key {i}'s NULL flag");
            for (j, b) in nulls.iter().enumerate() {
                assert_eq!(k(a) == k(b), i == j, "NULL in key {i} vs key {j}");
            }
            assert_ne!(k(a), k(&full), "a NULL key {i} meets no value");
        }
        assert_eq!(k(&rec([0, 0, 0, 3, 4], 0))[w], 0, "a zero image with no flag is a value, not a NULL");
    }

    /// CAP-28: past 64 columns the flags take one word per 64 keys (bit i % 64 of word i / 64, emit_image's): a NULL in key
    /// 0 and a NULL in key 64 (every other key 0) are two keys, as PG's set operations keep them apart; key 64 reads the
    /// second flags word, never key 0's bit, and a bit past the last key is masked off.
    #[test]
    fn a_set_key_past_64_columns_reads_one_flags_word_per_64_keys() {
        let arena = gravity_program::Arena::new(1 << 16);
        let l = gravity_program::record_layout(arena.handle(), &[KeyKind::I64; 67], &[SlotKind::Count], true);
        let (n, w) = (65, 65);
        let rec = |flags: [u64; 2]| {
            let mut r = vec![0u64; (l.size() as usize).div_ceil(8)]; // alloc: Cold
            (r[l.key_off(w) / 8], r[l.key_off(w + 1) / 8]) = (flags[0], flags[1]);
            r
        };
        // SAFETY: each record is a zeroed buffer of `l`'s size holding its 67 key words
        let k = |r: &Vec<u64>| unsafe { key(r.as_ptr() as *const u8, &l, n, w) };
        let (null0, null64) = (rec([1, 0]), rec([0, 1]));
        assert_eq!(k(&null0).len(), w + 2, "65 image words, then two flags words");
        assert_ne!(k(&null0), k(&null64), "a NULL in key 0 vs a NULL in key 64");
        assert_eq!(k(&rec([0, 0b11])), k(&null64), "a bit past key 64 is no key's");
    }

    /// F-ZERO: a plain side's implicit sentinel holds rows + 1 (`Rec::open`'s used mark): n rows count n, a record opened
    /// on no row counts 0, and a zeroed region never laid out counts 0.
    #[test]
    fn a_plain_side_counts_its_rows_not_the_used_mark() {
        let l = RecordLayout::new(&[], &[]);
        let (sent, j) = (l.sentinel_off() as usize, l.nslots() - 1);
        let mut buf = vec![0u64; (HashAgg::bytes(&l, 1) as usize).div_ceil(8).max(8)]; // alloc: Cold
        let (p, len) = (buf.as_mut_ptr() as *mut u8, buf.len() * 8);
        // SAFETY: `buf` is `len` zeroed, 8-aligned bytes, alive for the test
        let bytes = || unsafe { core::slice::from_raw_parts(p, len) };
        assert_eq!(region_count(bytes(), sent), Some(0), "a zeroed region never laid out");
        // SAFETY: as above: a record table of one for `l` in `buf`
        let t = unsafe { HashAgg::at(p, &l, 1, len as u32) };
        let rec = t.single();
        assert_eq!(region_count(bytes(), sent), Some(0), "a record opened on no row");
        for n in 1..=3 {
            rec.add_count(j, 1);
            assert_eq!(region_count(bytes(), sent), Some(n), "{n} rows");
        }
    }

    /// D2197: an InitPlan's or a pulled CTE's set op is deferred, and lowerv4's renumber_deferred renames its sides' slots
    /// (frame.rs rename_slot: the readers' consumes and the protos' `slot`, never `arg`). The right slot is the one its
    /// producer makes in the program, not `arg`'s stale copy (trunk's read, a slot never laid out: INTERSECT gave 0 rows,
    /// EXCEPT every left group); a peer with no slot, or one the emit does not read, is none (prepare's named error).
    #[test]
    fn d2197_the_right_slot_is_its_renamed_producers_not_args() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{Code, End, Layout, Pipeline, Program, Reason, WritePlan};
        unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
            0
        }
        let layout = Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let group = |s: u8| Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashAgg, 64).in_slot(s)), Reason::NoDrivingSite);
        let emit = |reads: &'static [u8]| Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(reads);
        let program = |ps: Vec<Pipeline>| Program::with_declared_pipelines(layout, &[], &[], WritePlan::SELECT, ps.into_iter().map(|p| (p, Code::hand(noop))));
        // as renumber_deferred leaves an InitPlan's INTERSECT: the left group 0 -> 2, the right 1 -> 3; `arg` still names 1
        let setop = |emit: u16, peer: u16| Proto::new(ProtoKind::SetOp, emit).peer(peer).slot(2).words(0, 1).arg(1 | 1 << 8);
        let renamed = program(vec![group(2), group(3), emit(&[2, 3])]); // alloc: Cold
        assert_eq!(super::right_slot(&setop(2, 1), &renamed), Some(3), "the producer's renamed slot, not arg's stale 1");
        // a top-level set op (nothing renumbered): the slot arg names too
        assert_eq!(super::right_slot(&setop(2, 1), &program(vec![group(0), group(1), emit(&[0, 1])])), Some(1)); // alloc: Cold
        // a peer that is not a breaker, one the emit does not read, one past the program: none
        let other = program(vec![group(2), group(3), group(4), emit(&[2, 3])]); // alloc: Cold
        assert_eq!([3, 2, 9].map(|peer| super::right_slot(&setop(3, peer), &other)), [None, None, None]);
        assert_eq!(super::right_slot(&setop(3, 1), &other), Some(3));
    }

    /// The right side's counts: a key seen twice (two owners' records of one group) sums, an absent key reads 0, and keys that
    /// differ only in their flags word or past their first word stay apart, at any fill up to the index's half.
    #[test]
    fn the_right_counts_sum_per_key_and_miss_as_zero() {
        let n = 5000usize;
        let a = gravity_fn::arena::OpArena::scratch_of(None, gravity_fn::arena::Kind::HashAgg);
        let mut c = super::Counts::with_capacity(&a, 4 * n, 2).expect("the index's room");
        let mut want = std::collections::HashMap::new(); // alloc: Cold
        for i in 0..n as u64 {
            for k in [[i, 0], [i, 1], [i << 40, 0]] {
                c.probe().copy_from_slice(&k);
                c.add(i as i64 + 1).expect("within the room");
                *want.entry(k).or_insert(0i64) += i as i64 + 1;
            }
            c.probe().copy_from_slice(&[i, 0]);
            c.add(3).expect("within the room");
            *want.get_mut(&[i, 0]).unwrap() += 3;
        }
        for (k, v) in &want {
            c.probe().copy_from_slice(k);
            assert_eq!(c.get(), *v, "{k:?}");
        }
        for k in [[n as u64, 0], [0, 2], [1, 1 << 63]] {
            c.probe().copy_from_slice(&k);
            assert_eq!(c.get(), 0, "{k:?} is no right key");
        }
    }

    /// PG's output counts: nodeSetOp.c set_output_count over (numLeft, numRight).
    #[test]
    fn the_copy_rule_is_pgs() {
        let cases = [(3, 2, [1, 2, 0, 1]), (1, 0, [0, 0, 1, 1]), (2, 5, [1, 2, 0, 0]), (0, 4, [0, 0, 0, 0])];
        for (l, r, want) in cases {
            for (cmd, w) in want.iter().enumerate() {
                assert_eq!(copies(cmd as u64, l, r), *w, "cmd {cmd} over ({l}, {r})");
            }
        }
    }
}
