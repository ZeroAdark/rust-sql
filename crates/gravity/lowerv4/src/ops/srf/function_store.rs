//! Form (c) of kind `srf.function` (M5-plan §3.1 (c), Decision 5; split from srf/function_scan.rs, main 09-26): a FROM
//! entry with any member that does not run inline (a library or C set function, a row type, a record) is read from PG's
//! own FunctionScan as a whole. The incumbent node (hosts v4/m5.rs `ScanStore`, H-c) fills its store at the first pull
//! with PG's calls (their modes, the strict-NULL rule, ROWS FROM's padding, the ordinality), and the shared reader
//! (srf/store_read.rs) hands its rows on, one window per host call, deforming only the columns the pipeline reads. The
//! step is leader-only (its pipeline is the FunctionScan's `src.result` row). D1874: a SubPlan in a call is the
//! program's: the step evaluates it on its row at the open and passes its value, which the store's own state reads in its
//! place (rerun/store_read.rs `fed`; hosts srf/store.rs `bare`).
//! Owner M5-SERIES.
use gravity_decompose::ops::shared::srf::SrfDesc;
use gravity_decompose::{expr, Op};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type};
use gravity_kernels::ops::srf::entries as host;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::step::{Expand, ExpandKind, ExprMech, StepMech};
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, ResumeWord};
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::srf::face::{RowType, StoreSource};
use crate::ops::srf::function_scan::plain_type;
use crate::rerun::store_read::{fed, fed_values, reads_of, Fed};

/// The refusal by name: a FROM call's column read (whole) that no descriptor types (D693, D1073); a dropped attribute of
/// the call's row type is read by no one (D3374), so this names a column the catalog does not type.
pub const UNTYPED: &str = "a whole-row reference to a FROM call with a column no descriptor types (a dropped column of its row type)";

/// Op `j` of pipeline `k` (the FROM entry `d`, range entry `varno`, `ncols` columns) read from the scan's store.
pub(crate) fn store_step<'g, 'p>(px: &mut Px<'g, 'p>, ex: &'static dyn ExprMech, (k, j): (usize, usize), d: &SrfDesc<'p>, (varno, ordinality, ncols): (i32, bool, usize), node: i32) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    // construct C2: each fed SubPlan's evaluation at the open was granted its licence (quiet_ahead's, or none at
    // PostgreSQL's point); the step keeps the compiled value
    let fed: PgVec<'g, Fed> = fed(px, ex, (k, j), d, node).into_iter().map(|(f, _licence)| f).collect_in(px.mcx());
    let ops = &gx.pipelines()[k].ops[j + 1..];
    let reads = reads_of(ops, varno, ncols);
    // each column's type: the call's own where it names it (a column definition list, one column of a plain type), the
    // ordinality's bigint, and a column the quals or the projection read from its Var; 0 where only the function's row type
    // knows it (the reader refuses such a column by name when it is read)
    let mcx = px.mcx();
    let mut types = mcx::pg_vec![in mcx; 0u32; ncols];
    let (mut a, mut composite) = (0, mcx::PgVec::new_in(mcx));
    for f in d.functions.iter() {
        let n = f.cols as usize;
        if f.coltypes.len() == n {
            f.coltypes.iter().enumerate().for_each(|(i, t)| types[a + i] = t);
        } else if let Some(t) = (n == 1).then(|| plain_type(px, ex, f, node)).flatten() {
            types[a] = t;
        } else {
            composite.push_in((a, n, f.node));
        }
        a += n;
    }
    if ordinality {
        types[ncols - 1] = types_core::INT8OID;
    }
    for op in ops {
        let mut var = |n| {
            expr::walk(n, &mut |x| {
                if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && (1..=ncols as i32).contains(&(v.varattno as i32))) {
                    types[v.varattno as usize - 1] = v.vartype;
                }
                true
            })
        };
        match op {
            Op::Filter(n) | Op::Gate(n) => _ = var(*n),
            Op::Project(l) => {
                l.iter().for_each(|n| _ = var(n));
                break;
            }
            _ => break,
        }
    }
    // D693: a composite call's columns no Var typed that the pipeline reads, its row type's attributes (hosts
    // `lookup_rowtype_tupdesc`), D1073: else its call's result descriptor (OUT parameters: hosts `get_expr_result_type`);
    // asked only then, so a call whose read columns the Vars type never asks the catalog
    let unread = |t: &[u32], a: usize| t[a] != 0 || (a < 64 && reads >> a & 1 == 0);
    // D3374 (producerchain_fn_copy): a dropped attribute of the call's row type (an attno under the expr catalog's count with
    // field type 0, as step/expr_row.rs `dropped_in` reads it) holds no value and no reader takes it: a whole row reads no dropped column
    // (its row site leaves it NULL, expr_row.rs `rewrite`), so it is never deformed, as a named tuplestore's dropped one
    // (store_read.rs); PG's store tuple holds it NULL and the host's deform steps over it
    let dropped_att = |t: u32, at: i16| {
        use gravity_expr::catalog::Support;
        gx.catalog.support(Support::Natts(t)).is_ok_and(|n| n >= at as u32) && gx.catalog.support(Support::Field { typid: t, attno: at }).is_ok_and(|f| f == 0)
    };
    let mut dropped = 0u64;
    for (a, n, fnode) in composite {
        if (a..a + n).all(|i| unread(&types, i)) {
            continue;
        }
        let t = ex.out_type(gx, fnode, node);
        for i in a..a + n {
            if types[i] == 0 {
                let at = (i - a) as i16 + 1;
                types[i] = gx.facts.row_att(t, at).map(|x| x.typid).or_else(|| gx.facts.call_col(fnode, at).map(|c| c.0)).unwrap_or(0);
                if types[i] == 0 && i < 64 && dropped_att(t, at) {
                    dropped |= 1 << i;
                }
            }
        }
    }
    let reads = reads & !dropped;
    // a column read that no descriptor types (no catalog: a dropped one of the call's row type is read by no one)
    if types.iter().enumerate().any(|(a, &t)| t == 0 && (a >= 64 || reads >> a & 1 == 1)) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(UNTYPED) });
    }
    let make = crate::frame::iface::store_maker(&node).0;
    let rd = make(px, RowType { k, j, member: 0, node, types, reads });
    let proto = px.proto_words(host::SETFN_WORDS);
    px.declare_proto(Proto::new(ProtoKind::SetFn, k as u16).words(proto, host::SETFN_WORDS).arg(host::setfn_arg(0, 0, Some(node as u32))));
    // D1085 (retired): an embedded inner's store D1657's once rule names (filled once, rewound per re-open) declares D439's
    // Volatile protocol, so a capped pull never fills it for an owner row PostgreSQL never pulls; tagged for the entry (D2044)
    if k >= gx.pipelines().len() && crate::ops::srf::reopen_once::reopen_once(gx, px.arena, k, d) {
        if crate::ops::srf::reopen_once::eager_build(gx, k) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(crate::ops::srf::reopen_once::STORE_OUTER_FIRST_BUILD) }); // in-row (D2107)
        }
        px.declare_proto(Proto::new(ProtoKind::Volatile, k as u16).arg(gravity_program::protos::VOLATILE_ONCE_STORE));
    }
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    // D2044 (D2031): a store over user code (a member or an argument past FIRST_NORMAL_OID) fills at an open only a row
    // PG pulls reaches: an embedded inner's, or a SubPlan's behind a gate on its owner's row (its pipeline is the
    // program's own); D439's Volatile protocol caps a pull at the rows it owes, so no fill runs or raises ahead of a
    // FETCH (a PL/pgSQL raise leaves its SPI connection pushed: exec runner `PlState`). One protocol per store: none when
    // its pipeline took one already (the once rule above, or rerun/store_read.rs for a store re-opened on its owner's
    // parameters). A top-level store fills at the first pull either way: the cap only sizes its calls
    let id = px.ident(k) as u16; // the pipeline its protocols are declared under (`Px::declare_proto`)
    if d.functions.iter().any(|f| crate::effects::user_fn(px.gx, f.node)) && !px.protos.iter().any(|p| p.kind == ProtoKind::Volatile && p.pipeline == id) {
        px.declare_proto(Proto::new(ProtoKind::Volatile, k as u16));
    }
    mcx::pg_box_dyn!(px.mcx(), StoreStep { rd, proto, offset, node, ex, fed } => dyn StepMech + 'g)
}

/// Form (c)'s step: the reader over the incumbent FunctionScan's store. The offset word counts the store rows handed on
/// (0: the row has not opened it), so a stop inside a window resumes at its row and a resume never re-reads the store.
/// D1874: `fed`, the calls' SubPlans, evaluated on the row at the open.
struct StoreStep<'g> {
    rd: PgBox<'g, dyn StoreSource + 'g>,
    proto: u16,
    offset: i32,
    node: i32,
    ex: &'static dyn ExprMech,
    fed: PgVec<'g, Fed>,
}

impl StepMech for StoreStep<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        self.rd.open_words(w);
        w.declare(OpenWord::open("srf.function store rows handed on", Base::Words, self.offset, Type::I64, 0));
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let mcx = env.mcx;
        let ctypes: mcx::PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(mcx);
        let win_t: mcx::PgVec<'_, Type> = [Type::I64].into_iter().chain(ctypes.iter().copied()).collect_in(mcx);
        // l_win(from, carry…): the window holding store row `from`, entered through the cancel poll (every window's);
        // l_row(j, carry…): its rows; l_end(carry…): the store is through
        let (l_poll, l_win) = cancel::header(b, env.ctx, env.l_cancel, &win_t[..], env.mcx);
        let (l_open, l_end, l_row) = (b.create_block(&[]), b.create_block(&ctypes[..]), b.create_block(&win_t[..]));
        let off = env.load_word(b, self.offset);
        let fresh = b.icmp(IntCond::Eq, off, env.zero);
        let from = crate::ops::srf::function_scan::resume_at(b, env, off);
        b.cond_br(fresh, (l_open, &[]), (l_poll, &mcx::concat_in(&[&[from][..], &row.carry.params()[..]], mcx)));
        // the open on this row: the scan's store at its SetFn word, rewound (a re-run scan's rescan; nothing before the
        // fill, which the first window's pull runs)
        b.switch_to(l_open);
        let h = b.load(Type::I64, env.params, 8 * (self.proto as usize + host::SETFN_W_NODE) as i32);
        let fed = fed_values(b, env, self.ex, self.node, row, &self.fed);
        self.rd.open(b, env, h, env.zero);
        self.rd.rewind_fed(b, env, &fed);
        b.br(l_poll, &mcx::concat_in(&[&[env.zero][..], &row.carry.params()[..]], mcx));
        b.switch_to(l_win);
        let p = mcx::to_vec_in(&b.block_params(l_win)[..], mcx);
        let l_eof = b.create_block(&[Type::I64]);
        let win = self.rd.window(b, env, l_eof, p[0]);
        let base = b.int_bin(IntBinOp::Sub, p[0], win.cursor);
        b.br(l_row, &mcx::concat_in(&[&[win.cursor][..], &p[1..]], mcx));
        b.switch_to(l_eof);
        b.br(l_end, &p[1..]);
        b.switch_to(l_row);
        b.mark_loop_header(l_row);
        env.bounded.push_in(l_row);
        let q = mcx::to_vec_in(&b.block_params(l_row)[..], mcx);
        let (at, carry) = (q[0], Carry::from_params(&q[1..], env.mcx));
        let (l_one, l_next) = (b.create_block(&[]), b.create_block(&[]));
        let more = b.icmp(IntCond::Ult, at, win.end);
        b.cond_br(more, (l_one, &[]), (l_next, &[]));
        b.switch_to(l_next);
        let from = b.int_bin(IntBinOp::Add, base, win.end);
        b.br(l_poll, &mcx::concat_in(&[&[from][..], &carry.params()[..]], mcx));
        b.switch_to(l_one);
        let r = self.rd.row(b, env, &win, at);
        let handed = b.int_bin(IntBinOp::Add, base, at);
        let handed = b.int_bin(IntBinOp::Add, handed, env.one);
        env.store_word(b, self.offset, handed);
        let out = RowVals { row: r.row, vals: r.vals, cols: r.cols, carry: carry.copy_in(env.mcx), next: row.at, at: row.at };
        env.memo.clear();
        let c2 = next(b, env, &out);
        env.memo.clear();
        let at1 = b.int_bin(IntBinOp::Add, at, env.one);
        b.br(l_row, &mcx::concat_in(&[&[at1][..], &c2.params()[..]], mcx));
        b.switch_to(l_end);
        env.store_word(b, self.offset, env.zero);
        Carry::from_params(b.block_params(l_end), env.mcx)
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Srf, offset: self.offset, held: 0 })
    }
}
