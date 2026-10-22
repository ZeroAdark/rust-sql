//! A publish on relfilenumber 0 (a relation without storage: a partitioned table, whose relam only names its
//! partitions' default) is refused before the publisher touches the directory: no stat, no list, no read. Without the
//! refusal the base walk listed `pgrc_0` under the zero locator's path (58030 "list_dir failed ... (errno 2)").

use super::*;
use crate::plan::PlanBuilder;
use crate::publish::{publish_compact, publish_compile, publish_ext, publish_freeze, NO_STORAGE};
use crate::seal::PartSpec;
use crate::walsink::NoWal;
use crate::{WriteError, WriteResult};

const NO_STORAGE_SPEC: PartSpec = PartSpec { spc: 0, db: 0, relfilenumber: 0, schema_fingerprint: 0 };
const ZERO_DIR: &str = "pg_tblspc/0/PG_18_202506291/0/pgrc_0";

fn refused<T: std::fmt::Debug>(what: &str, vfs: &MemVfs, out: WriteResult<T>) {
    assert_eq!(out.expect_err(what), WriteError::Contract { detail: NO_STORAGE }, "{what}");
    assert!(vfs.ops().is_empty(), "{what}: the directory is never touched: {:?}", vfs.ops());
}

#[test]
fn a_publish_on_relfilenumber_0_is_refused_before_any_directory_access() {
    let probe = Probe::new(TxnVerdict::Committed);
    let mut vfs = MemVfs::new();
    let out = publish_ext(&mut vfs, &mut NoWal, ZERO_DIR, &NO_STORAGE_SPEC, 100, &probe, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(()));
    refused("a segment activation's publish (publish_ext)", &vfs, out);
    let mut vfs = MemVfs::new();
    let out = publish_compile(&mut vfs, &mut NoWal, ZERO_DIR, &NO_STORAGE_SPEC, &[(1, &[(0, 3)][..])], 100, &probe, (None, None));
    refused("a tombstone compile", &vfs, out);
    let mut vfs = MemVfs::new();
    let out = publish_compact(&mut vfs, &mut NoWal, ZERO_DIR, &NO_STORAGE_SPEC, None, &[(1, 0)], 100, &probe, (None, None));
    refused("a compaction", &vfs, out);
    let mut vfs = MemVfs::new();
    let out = publish_freeze(&mut vfs, &mut NoWal, ZERO_DIR, &NO_STORAGE_SPEC, &probe, (None, None), 100);
    refused("VACUUM's freeze", &vfs, out);

    // the control: a relation with storage whose directory is missing gets as far as the walk's list, the 58030 shape
    let mut vfs = MemVfs::new();
    let spec = PartSpec { relfilenumber: RELFILENUMBER, ..NO_STORAGE_SPEC };
    let out = publish_ext(&mut vfs, &mut NoWal, ZERO_DIR, &spec, 100, &probe, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(()));
    assert!(matches!(out, Err(WriteError::Io { op: "list_dir", errno: libc::ENOENT, .. })), "{out:?}");
}

#[test]
fn a_table_with_storage_publishes_as_before() {
    let probe = Probe::new(TxnVerdict::Committed);
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(7, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    let first = finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: first.manifest.header.schema_fingerprint };
    let ops = vfs.ops().len();
    // the same publish under relfilenumber 0 is refused and leaves the table's directory as it was
    let out = publish_ext(&mut vfs, &mut NoWal, DIR, &PartSpec { relfilenumber: 0, ..spec }, 9, &probe, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(()));
    assert_eq!(out.expect_err("refused"), WriteError::Contract { detail: NO_STORAGE });
    assert_eq!(vfs.ops().len(), ops, "nothing touched");
    let next = publish_ext(&mut vfs, &mut NoWal, DIR, &spec, 9, &probe, (None, None), &mut |_: Option<&Manifest>, _: &mut PlanBuilder<'_>| Ok(())).expect("published");
    assert_eq!((next.gen, next.manifest.header.prev_gen), (first.gen + 1, first.gen));
}
