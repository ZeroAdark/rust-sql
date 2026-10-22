//! Kind `setop.setop`'s streamed form (D2801; decompose ops/setop/stream.rs plans it): a sorted INTERSECT / EXCEPT [ALL]
//! run as PostgreSQL 18.6's setop_retrieve_sorted runs it (nodeSetOp.c). The step sits in the left input's own pipeline,
//! after its projection, and reads the right input's spool through the shared pulled-run reader (face/pull.rs), so the
//! right input runs only as far as the walk fetches it, and a LIMIT, a cursor or the left's end stops both inputs where PG
//! stops them.
//!
//! - The left: a row whose keys equal the open group's counts into it (setop_load_group's `numLeft`); a row with other keys
//!   is PG's look-ahead row: the open group is decided and handed on first (`decide`, `group_out`), then the row opens the
//!   next group; the input's end decides the last group (`flush`). The first row fetches the right input's first row
//!   before it opens its group (PG's init reads the right's first tuple right after the left's), so an empty left never
//!   pulls.
//! - The right: open words hold the index of PG's next right tuple (`W_RNEXT`, fetched already), whether a right group is
//!   loaded (`W_RHAVE`, PG's !needGroup) and its row count (`W_RN`, 0 once the right is exhausted). Loading a group counts
//!   the rows equal to its first, fetching one past them (PG's look-ahead), each fetch a pull past the stored rows.
//! - The compare: the keys' datum words in KeySem's order (`By::Word`: a signed or unsigned compare of the word, `desc`),
//!   NULLS FIRST / LAST as the plan says and two NULLs equal (setop_compare_slots's ApplySortComparator). Left < right: the
//!   left group has no match (numRight 0); equal: both groups are used; greater: the right group is passed over. A text
//!   key (text / varchar under the form's collation, KeySem's comparator bttextcmp) is compared, for its order and for a
//!   group's rows alike, as its sort support compares two values (kernels ops/setop/raw.rs `StreamTextCmp`: one call per
//!   pair of non-NULL values, as PG's comparator), so a nondeterministic collation's equal values group as in PG. AGA-30:
//!   a by-reference key whose type's default btree comparator is a built-in (decompose `hosted_key`: numeric_cmp,
//!   bpcharcmp, uuid_cmp, interval_cmp, ...) is compared the same way by that comparator under the form's collation
//!   (raw.rs `StreamHostedCmp` through KeySem's bound-call table, its resolution id `keylane::native_cmp`'s), never by its bytes and never by user code.
//! - The copies (set_output_count): INTERSECT 1 when the groups matched, INTERSECT ALL min(numLeft, numRight), EXCEPT 1
//!   when they did not, EXCEPT ALL max(numLeft - numRight, 0). The row handed on is the left group's first row, rebuilt
//!   from its key words (every output column is a key), as PG returns the left group's first tuple. A held key's word (a
//!   text or comparator-ordered key) is the address of the step's copy of that row's value (kernels ops/setop/raw.rs
//!   `StreamKeyCopy`: a varlena's detoasted payload behind a 4-byte header; `StreamFixedCopy`: a fixed by-reference image;
//!   one buffer per held key, grown only for a larger value), since the left input moves past the
//!   row (its page unpinned) before the group is decided; the copy is rewritten only when the next group opens, after the
//!   group's row (and a stop's re-emit of it) is done, so the handed-on cell lives `Until::Row`.
//!
//! Stop and resume: an expanding step (H32). The offset word counts the copies handed on and `W_COPIES` keeps the decided
//! count (plus one; 0 before the group is decided), so a stop inside a copy re-enters at the look-ahead row (or the
//! input's end) and hands the rest on without walking the right input again. Serial: the walk reads its input's order.
//! Owner M11-MERGE (the family's maker dispatches here, ops/setop/lower.rs).
use gravity_decompose::catalog::{TEXTOID, VARCHAROID};
use gravity_decompose::ops::shared::setop::{SetCmd, SetDesc, SetForm};
use gravity_decompose::ops::shared::sort::EqKey;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::{Op, Source};
use gravity_fn::typesem::{By, KeyReq};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{ExprLife, ExprOut, Rep};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::cell::{Cell, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::pull::{Form, PullWords};
use crate::face::runs::{SlotRun, SlotRunImpl};
use crate::face::step::{Expand, ExpandKind, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, Exits, ResumeWord};
use crate::graph::Gx;

// The step's own open words (`WordKey::Open { face: Setop, k, j, i }`; face/pull.rs's take 5 to 7), key `i` at `W_KEYS + i`,
// then the `t`-th held key's copy buffer (pointer, capacity) at `W_KEYS + n + 2t` and the word after it.
const W_OPEN: u8 = 0;
const W_COUNT: u8 = 1;
const W_COPIES: u8 = 2;
const W_RNEXT: u8 = 3;
const W_RHAVE: u8 = 4;
const W_RN: u8 = 8;
const W_LNULLS: u8 = 9;
const W_KEYS: u8 = 10;

/// The right input the streamed form pulls (decompose plans no other): one relation scan's pipeline, filters and a
/// projection only, or a quiet Sort's emit over one (D2805: `sorted_emit`), outside every rerun set.
const RIGHT: &str = "a streamed sorted set operation whose right input is not one pulled relation scan outside every rerun set";

/// D2805 (D3377): pipeline `q` reads one Sort's records (its source that Sort's slot, the only slot it consumes) whose fill
/// is one relation scan pipeline: decompose stream.rs `sorted_scan`'s right input.
fn sorted_emit(gx: &Gx<'_, '_>, q: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    let p = &gx.pipelines()[q];
    let Source::Breaker { slot } = p.source else { return false };
    let Some(fill) = gx.life_of(q, slot).map(|l| l.0 as usize) else { return false };
    let f = &gx.pipelines()[fill];
    p.desc.consumes.len() == 1
        && p.desc.consumes[0] == slot
        && crate::frame::iface::end_breaker(gx, fill) == Some(BreakerKind::Sort)
        && matches!(f.source, Source::Range { .. } | Source::Index { .. } | Source::Bitmap { .. })
        && f.desc.consumes.is_empty()
        && f.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Sort(_)))
}

/// A key KeySem does not order by its native datum word, nor a text key by bttextcmp under the form's collation, nor a
/// by-reference key by its type's built-in comparator (decompose `hosted_key`).
const KEY: &str = "a streamed sorted set operation key that KeySem does not order by its datum word";

/// varlena.c's btree comparator of text (pg_proc 360): the text_ops family's support 1, which a text key's sort support
/// orders as.
const BTTEXTCMP: u32 = 360;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// A compared column: its resno, type, datum width in bytes and order (a signed word compare, DESC, NULLS FIRST); a held
/// key's comparator, collation and copy buffer (`held`), its facts for the datum read.
#[derive(Clone, Copy)]
struct Key {
    resno: i16,
    typid: u32,
    bytes: u8,
    signed: bool,
    desc: bool,
    nulls_first: bool,
    held: Option<Held>,
    tf: TypeFacts,
}

/// A held key, compared by a call and handed on from a copy: the collation it compares under, its comparator (`proc`: 0 a
/// text key's bttextcmp through `StreamTextCmp`, else the built-in comparator's resolution id through `StreamHostedCmp`),
/// its copy's form (`fixed`: 0 a varlena's payload, else a by-reference image of that many bytes) and the buffer holding
/// the open left group's copy of its value (frame words, byte offsets: its address and its byte capacity).
#[derive(Clone, Copy)]
struct Held {
    coll: u32,
    proc: u32,
    fixed: u16,
    w_ptr: i32,
    w_cap: i32,
}

struct Stream<'g> {
    node: i32,
    cmd: SetCmd,
    keys: PgVec<'g, Key>,
    run: PgBox<'g, dyn SlotRun + 'g>,
    pull: PullWords,
    /// The frame word counting the copies of the group's row handed on (H32).
    offset: i32,
    /// The open words' byte offsets by index (`W_*`).
    w: PgVec<'g, i32>,
    mcx: Mcx<'g>,
}

/// The step of the set operation at op `j` of pipeline `k` (the left input's pipeline).
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let Some(Op::SetOp(SetDesc::Set { cmd, right, keys, form: SetForm::Stream { nulls_first, coll }, .. })) = gx.pipelines()[k].ops.get(j) else {
        bug(node, "a streamed set operation step over another op")
    };
    let Some(life) = gx.life_of(k, *right) else { bug(node, "a streamed set operation without its right input") };
    let inner = life.0 as usize;
    let q = &gx.pipelines()[inner];
    let scan = matches!(q.source, Source::Range { .. } | Source::Index { .. } | Source::Bitmap { .. }) && q.desc.consumes.is_empty();
    // D2805 (D3377): or a quiet Sort's emit (decompose stream.rs `sorted_scan`): its fill, the pulled set's exclusive
    // producer, runs at the first pull, as PG's ExecSort sorts its input at the SetOp's first right fetch
    let sorted = sorted_emit(gx, inner);
    let plain = q.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(gravity_decompose::join::JoinDesc::Spool)));
    let once = !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)) || r.pipelines.contains(&(inner as u16)));
    if !((scan || sorted) && plain && once && gx.run_arms(inner).is_empty()) {
        shape(node, RIGHT);
    }
    let mut keys: PgVec<'_, Key> = keys.iter().enumerate().map(|(i, e)| key(gx, node, e, nulls_first >> i & 1 == 1, *coll)).collect_in(mcx);
    // every column a key, once (decompose's plan): the group's row is its keys
    let n = keys.len() as i16;
    if (1..=n).any(|r| keys.iter().filter(|k| k.resno == r).count() != 1) {
        bug(node, "a streamed set operation whose keys are not its columns");
    }
    // calm right code is read ahead in growing budgets (face/pull.rs), its raises held; any other is pulled one row per fetch
    // (a sorted emit reads stored records: calm as its quiet subtree, decompose's test)
    let form = if (sorted || crate::effects::visible::pull::calm(px, inner)) && crate::face::pull::unnested(gx, k) { Form::grows().unwrap_or(Form::Exact) } else { Form::Exact };
    let pull = crate::face::pull::declare(px, (Face::Setop, k, j), inner, form);
    let run = crate::frame::iface::kind_impl::<SlotRunImpl>(&node, KindId::RunsSort).0.pulled_reader(px, k, *right);
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    let held = keys.iter().filter(|k| k.held.is_some()).count();
    let w: PgVec<'_, i32> = (0..W_KEYS as usize + keys.len() + 2 * held).map(|i| px.frame_word(WordKey::Open { face: Face::Setop, k, j: crate::frame::px::key_j(j), i: i as u8 }) as i32 * 8).collect_in(mcx);
    let mut at = W_KEYS as usize + keys.len();
    for t in keys.iter_mut().filter_map(|k| k.held.as_mut()) {
        (t.w_ptr, t.w_cap) = (w[at], w[at + 1]);
        at += 2;
    }
    mcx::pg_box_dyn!(mcx, Stream { node, cmd: *cmd, keys, run, pull, offset, w, mcx } => dyn StepMech + 'g)
}

/// The right input's spool, its end filling the run as the step pulls it (face/slot_run.rs `pulled_end`).
pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    crate::frame::iface::kind_impl::<SlotRunImpl>(&node, KindId::RunsSort).0.pulled_end(px, k)
}

/// Key `e`'s order as KeySem resolves its ordering operator (a by-value word of the column's width, compared signed or
/// unsigned, DESC for its family's `>`; a text or varchar key by bttextcmp under the form's collation `coll`; AGA-30, a
/// by-reference key decompose's `hosted_key` admits by its type's built-in comparator, under `coll` when collatable), else
/// the refusal by name.
fn key(gx: &Gx<'_, '_>, node: i32, e: &EqKey, nulls_first: bool, coll: u32) -> Key {
    let Some(tf) = gravity_decompose::catalog::typefacts::facts_of(gx.facts, e.typid) else { shape(node, KEY) };
    let text = matches!(tf.base, TEXTOID | VARCHAROID) && coll != 0;
    let hosted = !matches!(tf.base, TEXTOID | VARCHAROID) && gravity_decompose::ops::setop::hosted_key(gx.facts, &tf);
    let kc = if text || hosted && tf.collatable { coll } else { 0 };
    let mut cx = mcx::MemoryContext::new_bump("setop stream key"); // alloc: Cold
    let ks = gravity_fn::typesem::resolve(cx.mcx(), &KeyReq::sort(e.opno).under(kc));
    cx.reset();
    let mk = |bytes, signed, desc, held| Key { resno: e.resno, typid: e.typid, bytes, signed, desc, nulls_first, held, tf };
    match ks {
        Ok(Ok(s)) => match s.by {
            By::Word { lw, rw, signed } if s.native && lw == rw && tf.byval && i16::from(lw) == tf.typlen && lw <= 8 => mk(lw, signed, s.desc, None),
            By::Fmgr if text && s.lt == TEXTOID && s.rt == TEXTOID && s.procs.cmp == BTTEXTCMP => mk(8, false, s.desc, Some(Held { coll, proc: 0, fixed: 0, w_ptr: 0, w_cap: 0 })),
            _ if hosted && s.lt == s.rt && s.procs.cmp != 0 && (kc != 0 || !tf.collatable) => {
                let proc = crate::rep::keylane::native_cmp(gx.catalog, node, s.procs.cmp, tf.typid);
                let fixed = if tf.typlen > 0 { tf.typlen as u16 } else { 0 };
                mk(8, false, s.desc, Some(Held { coll: kc, proc, fixed, w_ptr: 0, w_cap: 0 }))
            }
            _ => shape(node, KEY),
        },
        _ => shape(node, KEY),
    }
}

impl<'g> Stream<'g> {
    fn load(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u8) -> Value {
        env.load_word(b, self.w[i as usize])
    }

    fn store(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u8, v: Value) {
        env.store_word(b, self.w[i as usize], v)
    }

    /// Key `key`'s datum word in `row` (signed: sign-extended from its width, else zero-extended; a held key's the value's
    /// address) and its NULL flag.
    fn datum(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, key: &Key, row: &RowVals) -> (Value, Value) {
        let Some(c) = row.col(key.resno) else { bug(self.node, "a streamed set operation key past the row") };
        if key.held.is_some() {
            return crate::rep::datum(&mut env.lx(b, self.node), c, &key.tf);
        }
        let (w, null) = crate::rep::datum_word(&mut env.lx(b, self.node), c, key.bytes);
        let w = match b.value_type(w) {
            Type::I64 => w,
            Type::Bool => b.conv(ConvOp::Zext, Type::I64, w),
            _ => b.conv(if key.signed { ConvOp::Sext } else { ConvOp::Zext }, Type::I64, w),
        };
        if key.bytes >= 8 {
            return (w, null);
        }
        let sh = b.iconst(Type::I64, 64 - 8 * key.bytes as i64);
        let up = b.int_bin(IntBinOp::Shl, w, sh);
        (b.int_bin(if key.signed { IntBinOp::Sshr } else { IntBinOp::Ushr }, up, sh), null)
    }

    /// The keys of a row of the left input.
    fn left(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> PgVec<'g, (Value, Value)> {
        self.keys.iter().map(|k| self.datum(b, env, k, row)).collect_in(self.mcx)
    }

    /// The keys of right row `j` (stored: its fetch pulled it).
    fn right(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value, carry: &Carry, at: Value) -> PgVec<'g, (Value, Value)> {
        env.memo.clear(); // the walk's blocks do not dominate each other: no memoized read crosses them
        let p = self.run.fetch(b, env, j);
        let r = self.run.vals(b, env, p, carry.copy_in(env.mcx), at, at);
        self.keys.iter().map(|k| self.datum(b, env, k, &r)).collect_in(self.mcx)
    }

    /// The open left group's first row's keys, from its words.
    fn held(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> PgVec<'g, (Value, Value)> {
        let mask = self.load(b, env, W_LNULLS);
        (0..self.keys.len()).map(|i| {
            let w = env.load_word(b, self.w[W_KEYS as usize + i]);
            let sh = b.iconst(Type::I64, i as i64);
            let bit = b.int_bin(IntBinOp::Ushr, mask, sh);
            let bit = b.int_bin(IntBinOp::And, bit, env.one);
            (w, b.icmp(IntCond::Ne, bit, env.zero))
        }).collect_in(self.mcx)
    }

    /// Held key `t`'s order of two values when `gate` holds and both are non-NULL (`StreamTextCmp` for a text key,
    /// `StreamHostedCmp` for a comparator-ordered one: -1 / 0 / 1, `I64`), else 0 with no call; a compare that raised leaves
    /// through the raise exit (HOST, its error stashed).
    fn held_cmp(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &Held, gate: Value, (lw, ln): (Value, Value), (rw, rn): (Value, Value)) -> Value {
        let h = match t.proc {
            0 => b.declare_host_once("setop_stream_text_cmp", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<gravity_kernels::ops::setop::raw::StreamTextCmp> as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() }),
            _ => b.declare_host_once("setop_stream_hosted_cmp", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<crate::ops::setop::raw::StreamHostedCmp> as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() }),
        };
        let tr = b.bconst(true);
        let some = b.int_bin(IntBinOp::Or, ln, rn);
        let some = b.int_bin(IntBinOp::Xor, some, tr);
        let call = b.int_bin(IntBinOp::And, gate, some);
        let (l_call, l_ok, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(call, (l_call, &[]), (l_join, &[env.zero]));
        b.switch_to(l_call);
        // a text key's collation, or a comparator's resolution id with its collation above it (`StreamHostedCmp`)
        let arg = b.const_raw(Type::I64, u64::from(t.proc) | u64::from(t.coll) << if t.proc == 0 { 0 } else { 32 });
        let r = b.call_host(h, &[env.ctx, lw, rw, arg], Some(Type::I64)).expect("returns the order");
        let two = b.iconst(Type::I64, 2);
        let raised = b.icmp(IntCond::Eq, r, two);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(raised, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        b.br(l_join, &[r]);
        b.switch_to(l_join);
        b.block_params(l_join)[0]
    }

    /// Value `datum` (0 for a NULL) copied into held key `t`'s buffer (`StreamKeyCopy` for a varlena, `StreamFixedCopy` for
    /// a fixed by-reference image): the copy's address, 0 for a NULL; a failed detoast or pin leaves through the raise exit
    /// (HOST, its error stashed).
    fn copy(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &Held, datum: Value) -> Value {
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let Some(words) = env.words else { bug(self.node, "a streamed set operation key copy without the words site") };
        let at = b.ptr_add(words, env.zero, 1, t.w_ptr);
        let w = match t.fixed {
            0 => {
                let h = b.declare_host_once("setop_stream_key_copy", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry2::<gravity_kernels::ops::setop::raw::StreamKeyCopy> as *const () as usize, flags);
                b.call_host(h, &[env.ctx, at, datum], Some(Type::I64)).expect("returns the copy")
            }
            n => {
                let h = b.declare_host_once("setop_stream_fixed_copy", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<gravity_kernels::ops::setop::raw::StreamFixedCopy> as *const () as usize, flags);
                let len = b.iconst(Type::I64, i64::from(n));
                b.call_host(h, &[env.ctx, at, datum, len], Some(Type::I64)).expect("returns the copy")
            }
        };
        let (failed, l_ok) = (b.icmp(IntCond::Slt, w, env.zero), b.create_block(&[]));
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        w
    }

    /// Whether two rows' keys are equal: each pair both NULL, or both values with one word (a held key: values its
    /// compare calls equal, asked only while the keys before it are equal).
    fn same(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l: &[(Value, Value)], r: &[(Value, Value)]) -> Value {
        let (t, mut all) = (b.bconst(true), b.bconst(true));
        for (key, (&(lw, ln), &(rw, rn))) in self.keys.iter().zip(l.iter().zip(r)) {
            let words = match &key.held {
                Some(x) => {
                    let c = self.held_cmp(b, env, x, all, (lw, ln), (rw, rn));
                    b.icmp(IntCond::Eq, c, env.zero)
                }
                None => b.icmp(IntCond::Eq, lw, rw),
            };
            let both = b.int_bin(IntBinOp::And, ln, rn);
            let (lv, rv) = (b.int_bin(IntBinOp::Xor, ln, t), b.int_bin(IntBinOp::Xor, rn, t));
            let vals = b.int_bin(IntBinOp::And, lv, rv);
            let vals = b.int_bin(IntBinOp::And, vals, words);
            let eq = b.int_bin(IntBinOp::Or, both, vals);
            all = b.int_bin(IntBinOp::And, all, eq);
        }
        all
    }

    /// setop_compare_slots over the keys: -1, 0 or 1 (`I64`), the first unequal key deciding (a held key's compare asked
    /// only while the keys before it are equal, as PG's loop stops at the first).
    fn compare(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l: &[(Value, Value)], r: &[(Value, Value)]) -> Value {
        let (zero, one, minus) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1), b.iconst(Type::I64, -1));
        let (mut res, mut open) = (zero, b.bconst(true));
        for (i, key) in self.keys.iter().enumerate() {
            let ((lw, ln), (rw, rn)) = (l[i], r[i]);
            let v = match &key.held {
                Some(x) => {
                    let c = self.held_cmp(b, env, x, open, (lw, ln), (rw, rn));
                    if key.desc { b.int_bin(IntBinOp::Sub, zero, c) } else { c }
                }
                None => {
                    let lt = b.icmp(if key.signed { IntCond::Slt } else { IntCond::Ult }, lw, rw);
                    let gt = b.icmp(if key.signed { IntCond::Sgt } else { IntCond::Ugt }, lw, rw);
                    let (lo, hi) = if key.desc { (one, minus) } else { (minus, one) };
                    let v = b.select(gt, hi, zero);
                    b.select(lt, lo, v)
                }
            };
            // ApplySortComparator: two NULLs equal, a NULL first or last as the key says, DESC reversing values alone
            let (l_null, r_null) = if key.nulls_first { (minus, one) } else { (one, minus) };
            let when_l = b.select(rn, zero, l_null);
            let when_v = b.select(rn, r_null, v);
            let c = b.select(ln, when_l, when_v);
            let nz = b.icmp(IntCond::Ne, c, zero);
            let take = b.int_bin(IntBinOp::And, open, nz);
            res = b.select(take, c, res);
            let tr = b.bconst(true);
            let eq = b.int_bin(IntBinOp::Xor, nz, tr);
            open = b.int_bin(IntBinOp::And, open, eq);
        }
        res
    }

    /// The open left group decided against the right input: the right walked to the first group not below it (loading
    /// each group PG loads, passing over the ones below), and set_output_count's copies of its row (`I64`).
    fn decide(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, at: Value) -> Value {
        let (zero, one) = (env.zero, env.one);
        let [l_loop, l_load, l_exh, l_grp, l_eq, l_cmpr, l_cmp2, l_ge] = [(); 8].map(|_| b.create_block(&[]));
        let (l_scan, l_done, l_count) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
        b.br(l_loop, &[]);
        b.switch_to(l_loop);
        b.mark_loop_header(l_loop);
        env.bounded.push_in(l_loop);
        let have = self.load(b, env, W_RHAVE);
        let has = b.icmp(IntCond::Ne, have, zero);
        b.cond_br(has, (l_cmpr, &[]), (l_load, &[]));
        // setop_load_group(right): its first row is PG's next tuple, fetched already (none: the right is exhausted)
        b.switch_to(l_load);
        let s = self.load(b, env, W_RNEXT);
        let gone = self.pull.ended(b, env, &*self.run, s);
        b.cond_br(gone, (l_exh, &[]), (l_grp, &[]));
        b.switch_to(l_exh);
        self.store(b, env, W_RN, zero);
        self.store(b, env, W_RHAVE, one);
        b.br(l_cmpr, &[]);
        b.switch_to(l_grp);
        let s1 = b.int_bin(IntBinOp::Add, s, one);
        b.br(l_scan, &[s1]);
        // each next row fetched (a pull past the stored rows), counted while it equals the group's first
        b.switch_to(l_scan);
        b.mark_loop_header(l_scan);
        env.bounded.push_in(l_scan);
        let jv = b.block_params(l_scan)[0];
        let end = self.pull.ended(b, env, &*self.run, jv);
        b.cond_br(end, (l_done, &[jv]), (l_eq, &[]));
        b.switch_to(l_eq);
        let first = self.right(b, env, s, carry, at);
        let row = self.right(b, env, jv, carry, at);
        let eq = self.same(b, env, &first[..], &row[..]);
        let j1 = b.int_bin(IntBinOp::Add, jv, one);
        b.cond_br(eq, (l_scan, &[j1]), (l_done, &[jv]));
        b.switch_to(l_done);
        let e = b.block_params(l_done)[0];
        let n = b.int_bin(IntBinOp::Sub, e, s);
        self.store(b, env, W_RN, n);
        self.store(b, env, W_RNEXT, e);
        self.store(b, env, W_RHAVE, one);
        b.br(l_cmpr, &[]);
        // the left group against the loaded right group
        b.switch_to(l_cmpr);
        let rn = self.load(b, env, W_RN);
        let none = b.icmp(IntCond::Eq, rn, zero);
        b.cond_br(none, (l_count, &[zero]), (l_cmp2, &[]));
        b.switch_to(l_cmp2);
        let next = self.load(b, env, W_RNEXT);
        let first = b.int_bin(IntBinOp::Sub, next, rn);
        let r = self.right(b, env, first, carry, at);
        let l = self.held(b, env);
        let c = self.compare(b, env, &l[..], &r[..]);
        let (lt, eq) = (b.icmp(IntCond::Slt, c, zero), b.icmp(IntCond::Eq, c, zero));
        b.cond_br(lt, (l_count, &[zero]), (l_ge, &[]));
        // equal: both groups are used; greater: the right group is passed over. The right needs its next group either way
        b.switch_to(l_ge);
        self.store(b, env, W_RHAVE, zero);
        b.cond_br(eq, (l_count, &[rn]), (l_loop, &[]));
        b.switch_to(l_count);
        let nr = b.block_params(l_count)[0];
        let nl = self.load(b, env, W_COUNT);
        match self.cmd {
            SetCmd::Intersect => {
                let m = b.icmp(IntCond::Sgt, nr, zero);
                b.conv(ConvOp::Zext, Type::I64, m)
            }
            SetCmd::IntersectAll => b.int_bin(IntBinOp::Smin, nl, nr),
            SetCmd::Except => {
                let m = b.icmp(IntCond::Eq, nr, zero);
                b.conv(ConvOp::Zext, Type::I64, m)
            }
            SetCmd::ExceptAll => {
                let d = b.int_bin(IntBinOp::Sub, nl, nr);
                b.int_bin(IntBinOp::Smax, d, zero)
            }
        }
    }

    /// The open left group's row: its first row's keys as word cells, each at its column.
    fn group_row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, carry: Carry<'e>, at: Value) -> RowVals<'e> {
        let held = self.held(b, env);
        let rep = |k: &Key| match k.held {
            Some(Held { fixed: 0, .. }) => Rep::VarlenaPtr,
            Some(Held { fixed, .. }) => Rep::FixedPtr { len: fixed },
            None => Rep::Word { bytes: k.bytes },
        };
        let cells = self.keys.iter().map(|k| Cell::of_expr(ExprOut { rep: rep(k), typid: k.typid, life: ExprLife::Row }, 0, 0, Nullable::Maybe)).collect_in(env.mcx);
        let vals = held.into_iter().map(|(w, null)| (w, Some(null))).collect_in(env.mcx);
        let mut cols = mcx::pg_vec![in env.mcx; None; self.keys.len()];
        for (i, k) in self.keys.iter().enumerate() {
            cols[k.resno as usize - 1] = Some(i as u16);
        }
        RowVals { row: Row { cells }, vals, cols, carry, next: at, at }
    }

    /// The open group decided (unless a stop inside its copies left it decided) and its row handed on its copy count
    /// times, the offset word at the next copy while each runs on; both words 0 after the group.
    fn group_out<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, at: Value, next: Next<'_, 'e>) -> Carry<'e> {
        let ctypes: PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let decided = self.load(b, env, W_COPIES);
        let fresh = b.icmp(IntCond::Eq, decided, env.zero);
        let (l_dec, l_have) = (b.create_block(&[]), b.create_block(&ctypes[..]));
        b.cond_br(fresh, (l_dec, &[]), (l_have, &carry.params()));
        b.switch_to(l_dec);
        let n = self.decide(b, env, &carry, at);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        self.store(b, env, W_COPIES, n1);
        b.br(l_have, &carry.params());
        b.switch_to(l_have);
        let c = Carry::from_params(b.block_params(l_have), env.mcx);
        let d = self.load(b, env, W_COPIES);
        let n = b.int_bin(IntBinOp::Sub, d, env.one);
        let row = self.group_row(b, env, c.copy_in(env.mcx), at);
        let mut wtypes = mcx::pg_vec![in self.mcx; Type::I64];
        wtypes.extend_from_slice_in(&ctypes[..]);
        let (l_walk, l_one, l_end) = (b.create_block(&wtypes[..]), b.create_block(&[]), b.create_block(&ctypes[..]));
        let start = env.load_word(b, self.offset);
        let args: PgVec<'_, Value> = [start].into_iter().chain(c.params()).collect_in(self.mcx);
        b.br(l_walk, &args[..]);
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.push_in(l_walk);
        let ps = b.block_params(l_walk);
        let (i, cw) = (ps[0], Carry::from_params(&ps[1..], env.mcx));
        let more = b.icmp(IntCond::Slt, i, n);
        b.cond_br(more, (l_one, &[]), (l_end, &cw.params()));
        b.switch_to(l_one);
        let i1 = b.int_bin(IntBinOp::Add, i, env.one);
        env.store_word(b, self.offset, i1);
        // a stop after a copy leaves AT the look-ahead row (or the input's end), the offset word saying the copies handed on
        let out = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: cw, next: at, at };
        let c2 = next(b, env, &out);
        let args: PgVec<'_, Value> = [i1].into_iter().chain(c2.params()).collect_in(self.mcx);
        b.br(l_walk, &args[..]);
        b.switch_to(l_end);
        let ce = Carry::from_params(b.block_params(l_end), env.mcx);
        env.store_word(b, self.offset, env.zero);
        self.store(b, env, W_COPIES, env.zero);
        ce
    }

    /// The left group opened over a row's keys: its words (a held key's the address of its copy of the value, 0 for a
    /// NULL), numLeft 1, undecided.
    fn open_group(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l: &[(Value, Value)]) {
        let mut mask = env.zero;
        for (i, &(w, null)) in l.iter().enumerate() {
            let w = match &self.keys[i].held {
                Some(t) => {
                    let d = b.select(null, env.zero, w);
                    self.copy(b, env, t, d)
                }
                None => w,
            };
            env.store_word(b, self.w[W_KEYS as usize + i], w);
            let bit = b.conv(ConvOp::Zext, Type::I64, null);
            let sh = b.iconst(Type::I64, i as i64);
            let bit = b.int_bin(IntBinOp::Shl, bit, sh);
            mask = b.int_bin(IntBinOp::Or, mask, bit);
        }
        self.store(b, env, W_LNULLS, mask);
        self.store(b, env, W_COUNT, env.one);
        self.store(b, env, W_COPIES, env.zero);
        self.store(b, env, W_OPEN, env.one);
    }
}

impl StepMech for Stream<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        self.pull.open_words(w, ["setop.stream right rows", "setop.stream right want", "setop.stream right budget"]);
        w.declare(OpenWord::open("setop.stream copies", Base::Words, self.offset, Type::I64, 0));
        let own = [(W_OPEN, "setop.stream left group open"), (W_COUNT, "setop.stream left group rows"), (W_COPIES, "setop.stream decided copies"), (W_RNEXT, "setop.stream right next row"), (W_RHAVE, "setop.stream right group loaded"), (W_RN, "setop.stream right group rows"), (W_LNULLS, "setop.stream left group NULL keys")];
        for (i, what) in own {
            w.declare(OpenWord::open(what, Base::Words, self.w[i as usize], Type::I64, 0));
        }
        for i in 0..self.keys.len() {
            w.declare(OpenWord::open("setop.stream left group key", Base::Words, self.w[W_KEYS as usize + i], Type::I64, 0));
        }
        // a held key's copy buffer: none (0) at every open, pinned at its first copy
        for t in self.keys.iter().filter_map(|k| k.held.as_ref()) {
            w.declare(OpenWord::open("setop.stream left group key copy", Base::Words, t.w_ptr, Type::I64, 0));
            w.declare(OpenWord::open("setop.stream left group key copy capacity", Base::Words, t.w_cap, Type::I64, 0));
        }
    }

    /// The right input's run as stored at each window: its row count kept for the pulls.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let n = self.run.open(b, env, x);
        self.pull.keep(b, env, n);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let memo = env.memo.set_aside();
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let cp = row.carry.params();
        let l = self.left(b, env, row);
        let open = self.load(b, env, W_OPEN);
        let is_open = b.icmp(IntCond::Ne, open, env.zero);
        let [l_first, l_cmp, l_same, l_new] = [(); 4].map(|_| b.create_block(&[]));
        let (l_open, l_out) = (b.create_block(&ctypes[..]), b.create_block(&ctypes[..]));
        b.cond_br(is_open, (l_cmp, &[]), (l_first, &[]));
        // the first left row: PG's init fetches the right input's first row, then the row opens the first group
        b.switch_to(l_first);
        let z = env.zero;
        let _ = self.pull.ended(b, env, &*self.run, z);
        b.br(l_open, &cp);
        b.switch_to(l_cmp);
        let h = self.held(b, env);
        let same = self.same(b, env, &h[..], &l[..]);
        b.cond_br(same, (l_same, &[]), (l_new, &[]));
        b.switch_to(l_same);
        let n = self.load(b, env, W_COUNT);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        self.store(b, env, W_COUNT, n1);
        b.br(l_out, &cp);
        // PG's look-ahead row: the open group decided and handed on first
        b.switch_to(l_new);
        let c = self.group_out(b, env, row.carry.copy_in(env.mcx), row.at, next);
        b.br(l_open, &c.params());
        b.switch_to(l_open);
        let c = Carry::from_params(b.block_params(l_open), env.mcx);
        self.open_group(b, env, &l[..]);
        b.br(l_out, &c.params());
        b.switch_to(l_out);
        env.memo.restore(memo);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Chain, offset: self.offset, held: 0 })
    }

    fn flushes(&self) -> bool {
        true
    }

    /// The input's end decides the open group and hands it on (an empty left: nothing, the right never pulled).
    fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
        let memo = env.memo.set_aside();
        let ctypes: PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let open = self.load(b, env, W_OPEN);
        let is_open = b.icmp(IntCond::Ne, open, env.zero);
        let (l_emit, l_join) = (b.create_block(&[]), b.create_block(&ctypes[..]));
        b.cond_br(is_open, (l_emit, &[]), (l_join, &carry.params()));
        b.switch_to(l_emit);
        let z = env.zero;
        let c = self.group_out(b, env, carry, z, next);
        self.store(b, env, W_OPEN, env.zero);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        env.memo.restore(memo);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    fn serial(&self) -> bool {
        true
    }
}
