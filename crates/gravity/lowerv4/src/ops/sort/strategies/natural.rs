//! Road `sort.natural` (cards/M3.md L4, H26a): observed natural runs skip their sort. The run's first row sets
//! `RUN_ORDERED` in its `pad[2]` (with `RUN_KEYLANE` when road sort.keylane applies); every later row, while the bit
//! holds, compares its key pairs with the row before it and clears the bit at the first row that orders before it (a row
//! whose words tie through a key with an image tail clears it too, whatever a later key's words say: that tail, a
//! `name`'s image or a varlena's payload, decides the order and the pairs cannot tell). A run that keeps the bit is put
//! or sealed as it lies (`sort_full_res` / `sort_last_res`, kernels host_abi/v4/m3_sort.rs). Rows equal in every key keep
//! the bit: the SQL leaves their order open. Budget 160 lines. Owner M3-SORT.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::entries::{RUN_KEYLANE, RUN_ORDERED};

use crate::frame::env::Env;
use crate::ops::Table;

crate::strategies! {
    /// Observed natural runs skip their sort.
    SORT_NATURAL = 69, "sort.natural", Sort;
}

pub(crate) const ROWS: Table = Table { strategies: &[(SORT_NATURAL, &())], ..crate::ops::NONE };

/// The byte offset of `SortRun::pad[2]`.
pub(crate) const PAD2: i32 = 14;

/// The run's `pad[2]` bits at the row being stored at `at` (row `len` of `run`, `rb` bytes a row, `pairs` its key pairs):
/// set at the first row, `RUN_ORDERED` cleared at a row out of order. `tails[i]`: key `i` has an image tail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mark(b: &mut FunctionBuilder<'_>, env: &Env, run: Value, at: Value, len: Value, rb: Value, pairs: &[[Value; 2]], tails: &[bool], keylane: bool, natural: bool) {
    let bits = if keylane { RUN_KEYLANE } else { 0 } | if natural { RUN_ORDERED } else { 0 };
    if bits == 0 {
        return;
    }
    let (l_first, l_later, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let first = b.icmp(IntCond::Eq, len, env.zero);
    b.cond_br(first, (l_first, &[]), (l_later, &[]));
    b.switch_to(l_first);
    let v = b.iconst(Type::I8, bits as i64);
    b.store(run, PAD2, v);
    b.br(l_done, &[]);
    b.switch_to(l_later);
    if natural {
        let p2 = b.load_zext(Type::I8, run, PAD2);
        let ord = b.iconst(Type::I64, RUN_ORDERED as i64);
        let has = b.int_bin(IntBinOp::And, p2, ord);
        let (holds, l_check) = (b.icmp(IntCond::Ne, has, env.zero), b.create_block(&[]));
        b.cond_br(holds, (l_check, &[]), (l_done, &[]));
        b.switch_to(l_check);
        // lexicographic over the pair words, unsigned: `less` once a word orders before, `eq` while every word ties
        let neg = b.int_un(gravity_ir::IntUnOp::Neg, rb);
        let prev = b.ptr_add(at, neg, 1, 0);
        let (mut less, mut eq) = (b.bconst(false), b.bconst(true));
        let mut out = None;
        for (i, w) in pairs.iter().flatten().enumerate() {
            let p = b.load(Type::I64, prev, 8 * i as i32);
            let lt = b.icmp(IntCond::Ult, *w, p);
            let e = b.icmp(IntCond::Eq, *w, p);
            let both = b.int_bin(IntBinOp::And, eq, lt);
            less = b.int_bin(IntBinOp::Or, less, both);
            eq = b.int_bin(IntBinOp::And, eq, e);
            // the words tie through the first key with a tail: its tail, not a later key, orders the two rows
            if i % 2 == 1 && out.is_none() && tails.get(i / 2).copied().unwrap_or(false) {
                out = Some(eq);
            }
        }
        let out = out.map_or(less, |t| b.int_bin(IntBinOp::Or, less, t));
        let (l_clear, keep) = (b.create_block(&[]), b.iconst(Type::I64, !(RUN_ORDERED as i64) & 0xff));
        b.cond_br(out, (l_clear, &[]), (l_done, &[]));
        b.switch_to(l_clear);
        let cleared = b.int_bin(IntBinOp::And, p2, keep);
        let c8 = b.conv(gravity_ir::ConvOp::Trunc, Type::I8, cleared);
        b.store(run, PAD2, c8);
    }
    b.br(l_done, &[]);
    b.switch_to(l_done);
}
