// D1071: a CoerceViaIO calls its argument type's output function and its result type's input function, and its
// volatility is theirs, as PostgreSQL's contain_volatile_functions reads it (check_functions_in_node: getTypeOutputInfo /
// getTypeInputInfo, then func_volatile of each). A user type's two functions come from the source's `Facts::type_io`
// (the server's pg_type row) and their provolatile from `Facts::func`; without them they are unknown, read as VOLATILE.
// regress-full alter_table-034 (`select * from my_locks order by 1`: max(mode::lockmodes) in a sorted group, lockmodes an
// enum) streams its group (Op::GroupRun) only while the argument reads as VOLATILE: under enum_in / enum_out (STABLE in
// pg_proc) it keeps the keyed form, and a user base type whose input function is VOLATILE streams.
use std::fs;

use gravity_decompose::effects::{FuncProps, Volatility};
use gravity_decompose::{corpus, decompose_in, Facts, Op};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

/// The `lockmodes` enum of the fixture's catalog (the CoerceViaIO's resulttype).
const LOCKMODES: u32 = 42301;
/// PostgreSQL's enum_in / enum_out.
const ENUM_IO: (u32, u32) = (3504, 3505);
/// A user base type's input / output functions (oids outside the builtin catalog).
const USER_IN: u32 = 99_001;
const USER_OUT: u32 = 99_002;

/// The fixture's facts, but for `lockmodes`'s I/O functions (`io`) and the one user function `volatile` names.
struct Io<'f> {
    fx: &'f corpus::FixtureFacts,
    io: Option<(u32, u32)>,
    volatile: Option<u32>,
}

impl Facts for Io<'_> {
    fn rel(&self, relid: u32) -> Option<gravity_decompose::RelFact> {
        self.fx.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<gravity_decompose::AttFact> {
        self.fx.att(relid, attno)
    }
    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.fx.att_range(relid, attno)
    }
    fn index(&self, indexid: u32) -> Option<gravity_decompose::IndexFact> {
        self.fx.index(indexid)
    }
    fn index_am(&self, indexid: u32) -> gravity_decompose::IndexAm {
        self.fx.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(gravity_decompose::IndexFact)) {
        self.fx.indexes_of(relid, f)
    }
    fn index_ext(&self, indexid: u32) -> gravity_decompose::catalog::IndexExt {
        self.fx.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.fx.att_generated(relid, attno)
    }
    fn key(&self, req: gravity_decompose::catalog::KeyReq) -> Option<gravity_decompose::catalog::KeyOrder> {
        self.fx.key(req)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.fx.op_image_eq(opno)
    }
    fn instead_of(&self, relid: u32) -> gravity_decompose::InsteadOf {
        self.fx.instead_of(relid)
    }
    fn trig_ext(&self, relid: u32) -> gravity_decompose::ops::modifytable::trig_facts::TrigExt {
        self.fx.trig_ext(relid)
    }
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(gravity_decompose::catalog::LeafFact)) -> bool {
        self.fx.partition_leaves(root, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<gravity_decompose::ExprFp> {
        self.fx.index_expr(indexid, col)
    }
    fn type_io(&self, typid: u32) -> Option<(u32, u32)> {
        if typid == LOCKMODES { self.io } else { self.fx.type_io(typid) }
    }
    fn func(&self, funcid: u32) -> Option<FuncProps> {
        match funcid {
            f if Some(f) == self.volatile => Some(FuncProps { volatility: Volatility::Volatile, strict: true, leakproof: false, parallel: gravity_decompose::effects::Parallel::Unsafe }),
            USER_IN | USER_OUT => Some(FuncProps { volatility: Volatility::Immutable, strict: true, leakproof: false, parallel: gravity_decompose::effects::Parallel::Unsafe }),
            f => self.fx.func(f),
        }
    }
}

/// Whether alter_table-034's sorted group streams under `io` (lockmodes's I/O functions) and `volatile`.
fn streams(io: Option<(u32, u32)>, volatile: Option<u32>) -> bool {
    let text = fs::read_to_string(harness::plans_root().join("regress-full/alter_table-034.plan")).unwrap();
    assert!(text.contains(&format!(":resulttype {LOCKMODES}")), "the fixture casts to lockmodes ({LOCKMODES})");
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let facts = Io { fx: &fx.facts, io, volatile };
    let g = decompose_in(arena.handle(), fx.stmt, &facts, fx.mcx).unwrap_or_else(|e| panic!("{e}"));
    g.pipelines.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::GroupRun(_))))
}

#[test]
fn d1071_an_enum_cast_is_stable_and_keeps_the_keyed_form() {
    assert!(!streams(Some(ENUM_IO), None), "enum_in / enum_out are STABLE: max(mode::lockmodes) calls nothing VOLATILE");
}

#[test]
fn d1071_a_user_type_whose_input_function_is_volatile_streams() {
    assert!(streams(Some((USER_IN, USER_OUT)), Some(USER_IN)), "a VOLATILE typinput makes the cast VOLATILE");
    assert!(!streams(Some((USER_IN, USER_OUT)), None), "the same type with IMMUTABLE I/O functions calls nothing VOLATILE");
}

#[test]
fn d1071_a_user_type_without_its_io_functions_is_unknown_and_streams() {
    // a source that cannot read pg_type (a fixture without the row): unknown, read as VOLATILE, as the golden records
    assert!(streams(None, None));
}
