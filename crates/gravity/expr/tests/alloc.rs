//! Zero per-row allocation (docs/gravity/expr-compiler.md §3): `eval` over 4,096 rows allocates
//! nothing for a by-value result, and nothing for a by-reference result written into the region.

use std::alloc::{GlobalAlloc, Layout, System};

use datum::Datum;
use gravity_expr::{Expr, PgCatalog, Region, Row};
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::Oid;
use types_nodes::list::NodeList;
use types_nodes::node_tree::Node;
use types_portal::params::ParamBind;

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

struct Counting;

thread_local! {
    /// Allocations made by THIS thread. A process-wide counter charged any other thread's allocations during the loop to
    /// it, most likely libtest's main thread first parking on its result channel (std's mpmc builds a waiter context)
    /// late on a loaded gate box: 4 counted on D266's gate 3ba689cae66-0924T034521, 0 of 150 reruns on that binary.
    /// It counts only the calling thread: a measured call that hands work to another thread is not covered.
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's allocation count so far (see ALLOCS).
fn allocs_here() -> usize {
    ALLOCS.with(|c| c.get())
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1)); // try_with: a thread tearing down its TLS still allocates
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const INT4OID: Oid = 23;
const BOOLOID: Oid = 16;
const TEXTOID: Oid = 25;

fn seams() {
    miscinit_seams::get_user_id::set(|| 10);
    aclchk_seams::object_aclcheck::set(|_, _, _, _| Ok(0));
    syscache_seams::lookup_pg_type_shape::set(|typid| {
        Ok(match typid {
            INT4OID => Some(types_tuple::PgTypeShape { typlen: 4, typbyval: true, typalign: b'i' as i8, typstorage: b'p' as i8, typcollation: 0 }),
            TEXTOID => Some(types_tuple::PgTypeShape { typlen: -1, typbyval: false, typalign: b'i' as i8, typstorage: b'x' as i8, typcollation: 100 }),
            BOOLOID => Some(types_tuple::PgTypeShape { typlen: 1, typbyval: true, typalign: b'c' as i8, typstorage: b'p' as i8, typcollation: 0 }),
            _ => None,
        })
    });
    syscache_seams::pg_type_typtype::set(|_| Ok(Some(b'b' as i8)));
    syscache_seams::pg_type_element_shape::set(|_| Ok(None));
    fmgr_core::init_seams();
}

fn int4(mcx: Mcx<'_>, v: i32) -> Node<'_> {
    Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(v), false, true).unwrap()
}

fn text<'m>(mcx: Mcx<'m>, s: &str) -> Node<'m> {
    let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 4).unwrap();
    mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + 4)).unwrap();
    mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
    Node::mk_const(mcx, TEXTOID, -1, 100, -1, Datum::from_usize(img.leak().as_ptr() as usize), false, false).unwrap()
}

fn op<'m>(mcx: Mcx<'m>, f: Oid, rt: Oid, coll: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: 0, opfuncid: f, opresulttype: rt, opretset: false, opcollid: 0, inputcollid: coll, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

#[test]
fn eval_allocates_nothing_per_row() {
    seams();
    let ctx = MemoryContext::new("alloc");
    let mcx = ctx.mcx();
    let var = Node::mk_var(mcx, 1, 1, INT4OID, -1, 0, 0).unwrap();
    let byval = op(mcx, 66, BOOLOID, 0, &[op(mcx, 177, INT4OID, 0, &[var, int4(mcx, 3)]), int4(mcx, 100)]);
    let byref = op(mcx, 1258, TEXTOID, 950, &[text(mcx, "hello "), op(mcx, 1258, TEXTOID, 950, &[text(mcx, "wor"), text(mcx, "ld")])]);
    let mut cv = Expr::compile(lists(), byval, &PgCatalog, ParamBind::NONE).unwrap();
    let mut cr = Expr::compile(lists(), byref, &PgCatalog, ParamBind::NONE).unwrap();
    let values: Vec<Datum> = (0..4096).map(Datum::from_i32).collect();
    let nulls = vec![false; 4096];
    let mut buf = vec![0u8; 1 << 16];
    let before = allocs_here();
    let mut trues = 0usize;
    let mut bytes = 0usize;
    for i in 0..4096 {
        let row = Row::scan(&values[i..i + 1], &nulls[i..i + 1]);
        let mut region = Region::over(&mut buf);
        let r = cv.eval(&row, &mut region).unwrap();
        trues += (!r.isnull && r.value.as_bool()) as usize;
        let t = cr.eval(&row, &mut region).unwrap();
        // SAFETY: a text result is a plain image in the region.
        bytes += unsafe { types_tuple::varatt::varsize_any(t.value.as_usize() as *const u8) };
    }
    let after = allocs_here();
    assert_eq!(trues, 97);
    assert_eq!(bytes, 4096 * (4 + 11));
    assert_eq!(after - before, 0, "allocations during 4096 evaluations");
}
