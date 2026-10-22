//! Kind `runs.group_distinct` (D1071, D1120): a streamed sorted group's DISTINCT aggregate, a per-group distinct set over
//! the arguments of the rows the stream folds. ops/group/stream.rs hands a `GroupForm::Distinct` stream to
//! ops/group/stream_distinct.rs, which reads this file's set through the face (`kind_impl::<DistinctSetImpl>`): PG's
//! GroupAggregate sorts each group's DISTINCT arguments at the group's end and folds each distinct value once
//! (nodeAgg.c process_ordered_aggregate_single); the stream offers each non-NULL argument once, as the row folds, and folds
//! it when the set did not hold it (the admitted folds are order-free, so first-seen order gives PG's result).
//! The set: open addressing over the argument's image words in four frame words (table, capacity, length, generation),
//! its table in statement-lived pinned chunks (gravity_abi::program::pin, table/emit_key.rs key_copy's pool); the insert is
//! the host `dset_insert` in the family's raw half (raw.rs). A group's open bumps the generation (an entry of another
//! generation is empty), so emptying costs nothing; a table past half full doubles into a new chunk (the old one is the
//! statement's). Its memory is O(the largest group's distinct count), where PG's per-group tuplesort spills past
//! work_mem: that grant form is D1169's.
use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};

use crate::face::kinds::KindId;
use crate::face::runs::{DistinctSetImpl, DistinctSetMech};
use crate::frame::env::Env;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RunsGroupDistinct, &DistinctSetImpl(&SET))], ..crate::ops::NONE };

/// The set's frame words (byte offsets): the table's address (0), its capacity (8: entries, a power of two), its length
/// and its generation.
const W_LEN: i32 = 16;
const W_GEN: i32 = 24;

struct Set;

const SET: Set = Set;

impl DistinctSetMech for Set {
    fn words(&self) -> u8 {
        4
    }

    fn clear(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value) {
        let g = b.load(Type::I64, set, W_GEN);
        let g = b.int_bin(IntBinOp::Add, g, env.one);
        b.store(set, W_GEN, g);
        b.store(set, W_LEN, env.zero);
    }

    fn insert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value, img: &[Value]) -> Value {
        let slot = b.declare_stack_slot(8 * img.len() as u32, 8);
        let key = b.stack_addr(slot);
        for (i, &w) in img.iter().enumerate() {
            b.store(key, 8 * i as i32, w);
        }
        let h = b.declare_host_once("dset_insert", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), super::raw::dset_insert as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
        let n = b.iconst(Type::I64, img.len() as i64);
        let r = b.call_host(h, &[env.ctx, set, key, n], Some(Type::I64)).expect("dset_insert returns its status");
        let (failed, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        b.icmp(IntCond::Ne, r, env.zero)
    }

    /// RUNS-33: equal arguments adjacent, a value is new when it differs from the last one kept. An image of up to two
    /// words is kept in the table's and the capacity's words (no table is pinned: the statement offers sorted only), the
    /// length word saying whether the group kept one; a wider image takes the set.
    fn insert_sorted(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value, img: &[Value]) -> Value {
        if img.len() > 2 {
            return self.insert(b, env, set, img);
        }
        let kept = b.load(Type::I64, set, W_LEN);
        let mut new = b.icmp(IntCond::Eq, kept, env.zero);
        for (i, &w) in img.iter().enumerate() {
            let last = b.load(Type::I64, set, 8 * i as i32);
            let other = b.icmp(IntCond::Ne, last, w);
            new = b.int_bin(IntBinOp::Or, new, other);
            b.store(set, 8 * i as i32, w);
        }
        b.store(set, W_LEN, env.one);
        new
    }
}
