use super::*;
use mcx::MemoryContext;
use std::sync::Once;
use types_nodes::{Node, NodeList};

const SPC_WITH_OPTS: Oid = 7001;
const SPC_NULL_OPTS: Oid = 7002;
const SPC_SHORT_OPTS: Oid = 7003;

static SEAMS: Once = Once::new();

fn def<'mcx>(mcx: Mcx<'mcx>, name: &'static str, val: &'static str) -> Node<'mcx> {
    let arg = Node::mk(mcx, ::types_nodes::String { sval: val }).unwrap();
    Node::mk(
        mcx,
        ::types_nodes::parsenodes::DefElem {
            defnamespace: None,
            defname: Some(name),
            arg: Some(arg),
            defaction: ::types_nodes::parsenodes::DefElemAction::DEFELEM_UNSPEC,
            location: -1,
        },
    )
    .unwrap()
}

// {seq_page_cost=1.1} as stored on disk: a 1-byte-header text[] image.
fn short_header_options() -> Vec<u8> {
    let elem = b"seq_page_cost=1.1";
    let mut body = Vec::new();
    body.extend_from_slice(&1i32.to_ne_bytes());
    body.extend_from_slice(&0i32.to_ne_bytes());
    body.extend_from_slice(&25u32.to_ne_bytes());
    body.extend_from_slice(&1i32.to_ne_bytes());
    body.extend_from_slice(&1i32.to_ne_bytes());
    body.extend_from_slice(&((4 + elem.len() as u32) << 2).to_ne_bytes());
    body.extend_from_slice(elem);
    while body.len() % 4 != 0 {
        body.push(0);
    }
    let mut img = vec![(((body.len() + 1) as u8) << 1) | 0x01];
    img.extend_from_slice(&body);
    img
}

fn install() {
    SEAMS.call_once(|| {
        syscache_seams::pg_tablespace_spcoptions::set(|mcx, spcid| {
            Ok(match spcid {
                SPC_WITH_OPTS => {
                    let list = NodeList::from_slice(
                        mcx,
                        &[
                            def(mcx, "random_page_cost", "1.5"),
                            def(mcx, "seq_page_cost", "2.5"),
                            def(mcx, "effective_io_concurrency", "7"),
                        ],
                    )?;
                    let img = reloptions::transformRelOptions(
                        mcx, None, &list, None, &[], false, false,
                    )?
                    .expect("options image");
                    Some(Some(img))
                }
                SPC_SHORT_OPTS => Some(Some(mcx::slice_in(mcx, &short_header_options())?)),
                SPC_NULL_OPTS => Some(None),
                _ => None,
            })
        });
        guc_tables::vars::effective_io_concurrency
            .install_if_absent(guc_tables::GucVarAccessors { get: || 16, set: |_| {} });
        guc_tables::vars::maintenance_io_concurrency
            .install_if_absent(guc_tables::GucVarAccessors { get: || 10, set: |_| {} });
    });
}

// spccache.c:98-170: a present spcoptions parses (unset members are -1), a
// null spcoptions and a missing pg_tablespace row both read as no options,
// and InvalidOid resolves to MyDatabaseTableSpace (InvalidOid here: no row).
#[test]
fn get_tablespace_parses_present_null_and_missing_options() {
    install();
    let cx = MemoryContext::new("t");
    let m = cx.mcx();
    let opts = get_tablespace(m, SPC_WITH_OPTS).unwrap().unwrap();
    assert_eq!(opts.random_page_cost, 1.5);
    assert_eq!(opts.seq_page_cost, 2.5);
    assert_eq!(opts.effective_io_concurrency, 7);
    assert_eq!(opts.maintenance_io_concurrency, -1);
    assert!(get_tablespace(m, SPC_NULL_OPTS).unwrap().is_none());
    assert!(get_tablespace(m, 9).unwrap().is_none());
    assert!(get_tablespace(m, InvalidOid).unwrap().is_none());
}

// spccache.c:182-205: set options win, unset ones (< 0 or no options) fall
// back to the GUC values.
#[test]
fn page_costs_prefer_tablespace_options_over_gucs() {
    install();
    let cx = MemoryContext::new("t");
    let m = cx.mcx();
    assert_eq!(get_tablespace_page_costs(m, SPC_WITH_OPTS, 4.0, 1.0).unwrap(), (1.5, 2.5));
    assert_eq!(get_tablespace_page_costs(m, SPC_NULL_OPTS, 4.0, 1.0).unwrap(), (4.0, 1.0));
    assert_eq!(get_tablespace_page_costs(m, 9, 4.0, 1.0).unwrap(), (4.0, 1.0));
}

// spccache.c:215-237: per-tablespace io concurrency, GUC fallback when unset.
#[test]
fn io_concurrency_prefers_tablespace_options_over_gucs() {
    install();
    let cx = MemoryContext::new("t");
    let m = cx.mcx();
    assert_eq!(get_tablespace_io_concurrency(m, SPC_WITH_OPTS).unwrap(), 7);
    assert_eq!(get_tablespace_io_concurrency(m, SPC_NULL_OPTS).unwrap(), 16);
    assert_eq!(get_tablespace_maintenance_io_concurrency(m, SPC_WITH_OPTS).unwrap(), 10);
    assert_eq!(get_tablespace_maintenance_io_concurrency(m, 9).unwrap(), 10);
}

// Read streams look options up in per-page scratch that is reset with a leak check.
#[test]
fn lookup_leaves_nothing_charged_in_the_callers_context() {
    install();
    let mut cx = MemoryContext::new("btvacuumpage");
    let m = cx.mcx();
    let opts = get_tablespace(m, SPC_SHORT_OPTS).unwrap().unwrap();
    assert_eq!(opts.seq_page_cost, 1.1);
    assert_eq!(opts.random_page_cost, -1.0);
    assert_eq!(get_tablespace_page_costs(m, SPC_SHORT_OPTS, 4.0, 1.0).unwrap(), (4.0, 1.1));
    assert_eq!(get_tablespace_maintenance_io_concurrency(m, SPC_SHORT_OPTS).unwrap(), 10);
    assert_eq!(cx.used(), 0);
    cx.reset();
}
