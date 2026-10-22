//! A set function a host runs over its cell (M5-HOSTS H-a / H-b / H-f, kernels host_abi v4/m5.rs; M5-plan §6): a library
//! entry (a one-column row), `unnest` of one array, or PG's own call through `FmgrSet` (ValuePerCall or a
//! Materialize-mode hand-back; a composite as one record datum), which the execution builds from its target entry (the
//! handle, by resno: the program keeps no pointer and stays cacheable). One SetFn protocol per step reserves its slots and
//! cells per execution; a cell holds the state, the row and the arguments, so a stop keeps the set and a resume never
//! re-opens it. A SQL / PL body runs nested (V4R-295: its FROM form is srf.pl's). Leader-only (hir.rs `srf_serial`, D686). Owner M5-PSET; budget 160.
use gravity_decompose::ops::shared::srf::{SrfFn, SrfKind};
use gravity_fn::srf::{ENTRIES, STATE_SLOT};
use gravity_fn::srf_fmgr::FmgrSet;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::srf::entries as h;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, Mcx, PgVec};

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::storage;

#[derive(Clone, Copy)]
enum Host {
    Lib { entry: u64, coll: u64 },
    Array { facts: u64 },
    Fmgr { resno: u64 },
}

/// One host set: its cell at byte `off` of the step's cells, its slot `slot` of the step's reserved ones.
pub(crate) struct Member {
    host: Host,
    nargs: usize,
    off: i32,
    slot: u16,
}

/// One step's cells and its SetFn protocol's words.
#[derive(Default)]
pub(crate) struct Cells {
    bytes: u32,
    slots: u16,
    base: Option<u16>,
    node: Option<u32>,
    private: u64,
}

/// This execution's cells, first slot and PG-call handle.
#[derive(Clone, Copy)]
pub(crate) struct Words {
    cells: Value,
    slots: Value,
    h: Value,
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

impl Cells {
    /// `f` (result type `typ`) as a host set of this step.
    pub(crate) fn member<'p>(&mut self, px: &mut Px<'_, 'p>, f: &SrfFn<'p>, typ: u32, node: i32) -> Member {
        let cat = px.gx.catalog;
        let whole = storage::whole_row(cat, typ);
        let nargs = f.args.len();
        let coll = f.node.as_func_expr().map_or(0, |x| x.inputcollid);
        let host = match ENTRIES.iter().position(|e| e.oid == f.funcid && e.cols == 1) {
            _ if f.kind == SrfKind::Unnest && nargs == 1 => {
                let Ok(t) = storage::of(cat, typ) else { shape(node, "an unnest whose element type the catalog does not know") };
                fires::fire("srf.unnest_array");
                Host::Array { facts: t.len as u16 as u64 | (t.byval as u64) << h::FACT_BYVAL | (t.align as u8 as u64) << h::FACT_ALIGN }
            }
            Some(i) if !whole => Host::Lib { entry: i as u64, coll: coll as u64 },
            _ => {
                // the handle's set, built per execution from the step's target entry, hands a composite back whole
                FmgrSet::check(f.funcid, f.node, nargs, true, true).unwrap_or_else(|r| shape(node, r.what()));
                (self.node, self.private) = (Some(node as u32), h::SETFN_PRIVATE); // M5 Decision 7: the session's own
                Host::Fmgr { resno: f.resno as u64 }
            }
        };
        let (off, slot) = (self.bytes as i32, self.slots);
        self.bytes += (h::cell_bytes(nargs) as u32).next_multiple_of(64);
        self.slots += 1;
        Member { host, nargs, off, slot }
    }

    /// The step's SetFn protocol, once its members are known.
    pub(crate) fn declare(&mut self, px: &mut Px<'_, '_>, k: usize) {
        if self.slots == 0 { return }
        let mut base = px.proto_words(h::SETFN_WORDS);
        if base == 0 {
            // word 0 stays "none" (keyed/table_size.rs)
            base = px.proto_words(h::SETFN_WORDS);
        }
        px.declare_proto(Proto::new(ProtoKind::SetFn, k as u16).words(base, h::SETFN_WORDS).arg(h::setfn_arg(self.slots, self.bytes, self.node) | self.private));
        self.base = Some(base);
    }

    pub(crate) fn words(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Words> {
        let base = 8 * self.base? as i32;
        let cells = b.load(Type::I64, env.params, base + 8 * h::SETFN_W_CELLS as i32);
        let cells = b.conv(ConvOp::IntToPtr, Type::Ptr, cells);
        let slots = b.load(Type::I64, env.params, base + 8 * h::SETFN_W_BASE as i32);
        let h = self.node.map_or(env.zero, |_| b.load(Type::I64, env.params, base + 8 * h::SETFN_W_NODE as i32));
        Some(Words { cells, slots, h })
    }
}

/// A datum word: an 8-byte value as it is, a narrower integer widened (a bool 0 / 1), a float's bits.
fn datum(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    match b.value_type(v) {
        Type::Bool => b.conv(ConvOp::Zext, Type::I64, v),
        Type::I8 | Type::I16 | Type::I32 => b.conv(ConvOp::Sext, Type::I64, v),
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, v),
        Type::F32 => {
            let w = b.conv(ConvOp::Bitcast, Type::I32, v);
            b.conv(ConvOp::Zext, Type::I64, w)
        }
        Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, v),
        _ => v,
    }
}

fn call(b: &mut FunctionBuilder<'_>, mcx: Mcx<'_>, name: &str, addr: usize, args: &[Value]) -> Value {
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let params: PgVec<'_, Type> = args.iter().map(|&a| b.value_type(a)).collect_in(mcx);
    let f = b.declare_host_once(name, &params[..], Some(Type::I64), addr, flags);
    b.call_host(f, args, Some(Type::I64)).unwrap_or_else(|| b.iconst(Type::I64, h::SRF_FAILED as i64))
}

/// The array face's open: a NULL array (strict) opens no rows.
fn open_array(b: &mut FunctionBuilder<'_>, env: &Env, cell: Value, (v, null): (Value, Option<Value>), facts: u64) -> Value {
    let (l_open, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    match null { Some(null) => b.cond_br(null, (l_join, &[env.zero]), (l_open, &[])), None => b.br(l_open, &[]) }
    b.switch_to(l_open);
    let (d, fx) = (datum(b, v), b.iconst(Type::I64, facts as i64));
    let r = call(b, env.mcx, "srf_array_open", h::srf_array_open as *const () as usize, &[env.ctx, cell, d, fx]);
    b.br(l_join, &[r]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

impl Member {
    /// The open, on this row's arguments: 1 opened, 0 no rows (a strict entry's NULL argument, a NULL array), else the
    /// raise word (PG's error stashed).
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &Env, Words { cells, slots, h: hd }: Words, args: &[(Value, Option<Value>)]) -> Value {
        let slot = b.iconst(Type::I64, self.slot as i64);
        let slot = b.int_bin(IntBinOp::Add, slots, slot);
        b.store(cells, self.off + (h::CELL_STATE + 8 * STATE_SLOT) as i32, slot);
        let cell = b.ptr_add(cells, env.zero, 1, self.off);
        let n = b.iconst(Type::I64, self.nargs as i64);
        let (name, addr, a) = match self.host {
            Host::Array { facts } => return open_array(b, env, cell, args[0], facts),
            Host::Lib { entry, coll } => ("srf_open", h::srf_open as *const () as usize, [b.iconst(Type::I64, entry as i64), n, b.iconst(Type::I64, coll as i64)]),
            Host::Fmgr { resno } => ("fmgr_open", h::fmgr_open as *const () as usize, [hd, b.iconst(Type::I64, resno as i64), n]),
        };
        let nulls = self.off + h::cell_nulls(self.nargs) as i32;
        for (i, &(v, null)) in args.iter().enumerate() {
            let d = datum(b, v);
            b.store(cells, self.off + (h::CELL_ARGS + 8 * i) as i32, d);
            let nb = match null { Some(x) => b.conv(ConvOp::Zext, Type::I8, x), None => b.iconst(Type::I8, 0) };
            b.store(cells, nulls + i as i32, nb);
        }
        call(b, env.mcx, name, addr, &[env.ctx, cell, a[0], a[1], a[2]])
    }

    /// The set's next row into its cell: 1, 0 = the end, else the raise word.
    pub(crate) fn step(&self, b: &mut FunctionBuilder<'_>, env: &Env, w: Words) -> Value {
        let cell = b.ptr_add(w.cells, env.zero, 1, self.off);
        let (name, addr, a): (_, _, &[Value]) = match self.host {
            Host::Lib { entry, .. } => ("srf_step", h::srf_step as *const () as usize, &[env.ctx, cell, b.iconst(Type::I64, entry as i64)]),
            Host::Array { .. } => ("srf_unnest_step", h::srf_unnest_step as *const () as usize, &[env.ctx, cell]),
            Host::Fmgr { resno } => ("fmgr_step", h::fmgr_step as *const () as usize, &[env.ctx, cell, w.h, b.iconst(Type::I64, resno as i64)]),
        };
        call(b, env.mcx, name, addr, a)
    }

    /// The row's value: its datum word and whether it is NULL.
    pub(crate) fn value(&self, b: &mut FunctionBuilder<'_>, env: &Env, w: Words) -> (Value, Value) {
        let (d, null) = (b.load(Type::I64, w.cells, self.off + h::CELL_OUT as i32), b.load(Type::I64, w.cells, self.off + h::CELL_OUT as i32 + 8));
        (d, b.icmp(IntCond::Ne, null, env.zero))
    }
}
