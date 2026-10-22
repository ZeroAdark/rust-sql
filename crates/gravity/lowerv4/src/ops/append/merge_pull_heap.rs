//! The pull-form merge's choice of the window's row, PostgreSQL's MergeAppend's own (D2884; a helper of merge_pull.rs):
//! its binary heap of subplan indices (nodeMergeAppend.c over binaryheap.c). At the open each live arm's first row, the
//! arms added in arm order, then `binaryheap_build` (sift_down from the last parent to the root); each later window
//! refills the top's arm and `binaryheap_replace_first`s it (sift_down from the root), or `binaryheap_remove_first`s it
//! at its end (the last node to the root, then sift_down). sift_down takes the right child only where it orders strictly
//! first and stops where the node orders first or ties its child: so a run of keys equal across arms leaves in the heap's
//! order (over 4 arms 0, 1, 3, 2), which any consumer that shows a tie's member reads (DISTINCT ON, a window's peers).
//! The heap is `n` frame words of arm indices and a size word; an arm's held row is read at a computed address (the arms'
//! words lie at one stride apart: merge_pull.rs `make`). A window's sift_down is unrolled to the heap's depth (no loop,
//! no cancel check), the open's build loops over the parents under a checked header. Registers nothing. Owner M3-MERGE.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::frame::env::Env;
use crate::rep::keylane::Key;

/// The heap's frame words (byte offsets into the words site) and the arms' held rows: arm 0's words, every arm's at
/// `stride` words past the one before.
pub(crate) struct Heap<'g> {
    /// `H[0]`: `n` contiguous words, the arm at each heap position.
    pub h: i32,
    pub size: i32,
    pub n: usize,
    pub stride: i32,
    pub alive: i32,
    pub pairs: PgVec<'g, i32>,
    pub vals: PgVec<'g, i32>,
    pub nulls: PgVec<'g, i32>,
}

/// The heap's depth over `n` arms: the most levels a sift_down from the root descends.
fn depth(n: usize) -> usize {
    if n < 2 { 0 } else { (usize::BITS - 1 - n.leading_zeros()) as usize }
}

impl Heap<'_> {
    fn words(&self, env: &Env) -> Value {
        match env.words {
            Some(w) => w,
            None => crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a merge heap without the words site" }),
        }
    }

    fn at(&self, b: &mut FunctionBuilder<'_>, env: &Env, pos: Value) -> Value {
        let w = self.words(env);
        let p = b.ptr_add(w, pos, 8, self.h);
        b.load(Type::I64, p, 0)
    }

    fn set(&self, b: &mut FunctionBuilder<'_>, env: &Env, pos: Value, arm: Value) {
        let w = self.words(env);
        let p = b.ptr_add(w, pos, 8, self.h);
        b.store(p, 0, arm);
    }

    /// Arm `arm`'s held words' base: the words site moved by `arm` strides.
    fn row(&self, b: &mut FunctionBuilder<'_>, env: &Env, arm: Value) -> Value {
        let w = self.words(env);
        let s = b.iconst(Type::I64, self.stride as i64);
        let i = b.int_bin(IntBinOp::Mul, arm, s);
        b.ptr_add(w, i, 8, 0)
    }

    /// Arm `arm`'s held row is alive (a row held).
    pub(crate) fn alive(&self, b: &mut FunctionBuilder<'_>, env: &Env, arm: Value) -> Value {
        let r = self.row(b, env, arm);
        let a = b.load(Type::I64, r, self.alive);
        b.icmp(IntCond::Ne, a, env.zero)
    }

    /// Arm `arm`'s held columns: `n` datum words then `n` null words.
    pub(crate) fn cols<'e>(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, arm: Value) -> PgVec<'e, Value> {
        let r = self.row(b, env, arm);
        self.vals.iter().chain(self.nulls.iter()).map(|&o| b.load(Type::I64, r, o)).collect_in(env.mcx)
    }

    /// Whether arm `x`'s held row orders strictly before arm `y`'s (merge_pull_cmp.rs `less`: the pairs, then a tail key's
    /// comparator over the held copies). `skip`: the compare is moot (its tail comparator is not called).
    fn first(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[Key], x: Value, y: Value, skip: Value) -> Value {
        let n = self.vals.len();
        let (rx, ry) = (self.row(b, env, x), self.row(b, env, y));
        let px: PgVec<'_, Value> = self.pairs.iter().map(|&o| b.load(Type::I64, rx, o)).collect_in(env.mcx);
        let py: PgVec<'_, Value> = self.pairs.iter().map(|&o| b.load(Type::I64, ry, o)).collect_in(env.mcx);
        let (mut vx, mut nx, mut vy) = (mcx::pg_vec![in env.mcx; env.zero; n], mcx::pg_vec![in env.mcx; env.zero; n], mcx::pg_vec![in env.mcx; env.zero; n]);
        for k in keys.iter().filter(|k| k.tail != 0) {
            let c = k.col as usize;
            vx[c] = b.load(Type::I64, rx, self.vals[c]);
            nx[c] = b.load(Type::I64, rx, self.nulls[c]);
            vy[c] = b.load(Type::I64, ry, self.vals[c]);
        }
        crate::ops::append::merge_pull_cmp::less(b, env, keys, &px[..], &py[..], &vx[..], &nx[..], &vy[..], skip)
    }

    /// binaryheap.c `sift_down` from position `pos0` over a heap of `size`, unrolled to the heap's depth.
    fn sift(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[Key], pos0: Value, size: Value) {
        let node = self.at(b, env, pos0);
        let l_done = b.create_block(&[Type::I64]);
        let (two, f) = (b.iconst(Type::I64, 2), b.bconst(false));
        let mut pos = pos0;
        for _ in 0..depth(self.n) {
            let l2 = b.int_bin(IntBinOp::Mul, pos, two);
            let l = b.int_bin(IntBinOp::Add, l2, env.one);
            let has_l = b.icmp(IntCond::Slt, l, size);
            let l_body = b.create_block(&[]);
            b.cond_br(has_l, (l_body, &[]), (l_done, &[pos]));
            b.switch_to(l_body);
            let r = b.int_bin(IntBinOp::Add, l, env.one);
            let has_r = b.icmp(IntCond::Slt, r, size);
            let ri = b.select(has_r, r, l);
            let (hl, hr) = (self.at(b, env, l), self.at(b, env, ri));
            let t = b.bconst(true);
            let no_r = b.int_bin(IntBinOp::Xor, has_r, t);
            let rf = self.first(b, env, keys, hr, hl, no_r);
            let rfirst = b.int_bin(IntBinOp::And, has_r, rf);
            let swap = b.select(rfirst, r, l);
            let hs = b.select(rfirst, hr, hl);
            let mv = self.first(b, env, keys, hs, node, f);
            let l_move = b.create_block(&[]);
            b.cond_br(mv, (l_move, &[]), (l_done, &[pos]));
            b.switch_to(l_move);
            self.set(b, env, pos, hs);
            pos = swap;
        }
        b.br(l_done, &[pos]);
        b.switch_to(l_done);
        let at = b.block_params(l_done)[0];
        self.set(b, env, at, node);
    }

    /// After an open, every arm read: the live arms added in arm order (`binaryheap_add_unordered`), then
    /// `binaryheap_build`: sift_down from position (size - 2) / 2 down to 0.
    pub(crate) fn build(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[Key]) {
        let mut size = env.zero;
        for a in 0..self.n {
            let arm = b.iconst(Type::I64, a as i64);
            self.set(b, env, size, arm);
            let live = self.alive(b, env, arm);
            let inc = b.conv(gravity_ir::ConvOp::Zext, Type::I64, live);
            size = b.int_bin(IntBinOp::Add, size, inc);
        }
        env.store_word(b, self.size, size);
        if self.n < 2 {
            return;
        }
        let two = b.iconst(Type::I64, 2);
        let s2 = b.int_bin(IntBinOp::Sub, size, two);
        let first = b.int_bin(IntBinOp::Sdiv, s2, two);
        let (l_head, l_body) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[Type::I64], env.mcx);
        let l_built = b.create_block(&[]);
        b.br(l_head, &[first]);
        b.switch_to(l_body);
        let i = b.block_params(l_body)[0];
        let (go, l_sift) = (b.icmp(IntCond::Sge, i, env.zero), b.create_block(&[]));
        b.cond_br(go, (l_sift, &[]), (l_built, &[]));
        b.switch_to(l_sift);
        self.sift(b, env, keys, i, size);
        let i1 = b.int_bin(IntBinOp::Sub, i, env.one);
        b.br(l_head, &[i1]);
        b.switch_to(l_built);
    }

    /// After the top's arm read its next row: `binaryheap_replace_first` while the arm holds a row, else
    /// `binaryheap_remove_first` (the last position to the root, the heap one smaller, then sift_down).
    pub(crate) fn settle(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[Key]) {
        let size = env.load_word(b, self.size);
        let top = self.at(b, env, env.zero);
        let live = self.alive(b, env, top);
        let (l_live, l_gone, l_sift, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]));
        b.cond_br(live, (l_live, &[]), (l_gone, &[]));
        b.switch_to(l_live);
        b.br(l_sift, &[size]);
        b.switch_to(l_gone);
        let s1 = b.int_bin(IntBinOp::Sub, size, env.one);
        env.store_word(b, self.size, s1);
        let last = self.at(b, env, s1);
        self.set(b, env, env.zero, last);
        let some = b.icmp(IntCond::Sgt, s1, env.zero);
        b.cond_br(some, (l_sift, &[s1]), (l_done, &[]));
        b.switch_to(l_sift);
        let sz = b.block_params(l_sift)[0];
        self.sift(b, env, keys, env.zero, sz);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// The window's arm: the heap's top, -1 when the heap is empty.
    pub(crate) fn top(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let size = env.load_word(b, self.size);
        let some = b.icmp(IntCond::Sgt, size, env.zero);
        let top = self.at(b, env, env.zero);
        let none = b.iconst(Type::I64, -1);
        b.select(some, top, none)
    }
}
