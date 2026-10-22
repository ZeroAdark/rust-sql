//! P-1 L5 (TOMB 2's review of the rebase): a compile with an extension edit (`publish_compile_ext`) and an ext-only
//! publish (`publish_ext`) over a base that carries a stats root keep both extension records: `EXT_STATS` (3), the
//! root, carried unchanged since neither publish changes a part ident, and `EXT_TOMBS` (4), the edit's. Both orders,
//! from both base paths (the directory's generation, and the catalog's `BaseStats::Gen`). A table needs more than
//! `LOOSE_MAX` parts before it has a root, so the default e2e tables may never carry both records at once.

use super::*;
use crate::plan::PlanBuilder;
use crate::publish::{effective_manifest, publish_compile_ext, publish_ext, BaseGen, BaseStats, PublishOutcome};
use crate::seal::PartSpec;
use crate::stats::LOOSE_MAX;
use crate::walsink::NoWal;
use crate::WriteResult;
use pgrc_format::manifest::{stats_ref, tombs, with_tombs, Manifest, RetiredTombs, TombsRecord};

const ROWS: u64 = 10;

/// Parts 0, 1, ... published one per transaction (fxids from 100) until the generation carries a stats root: the
/// part spec, the last publish, and the next free fxid.
fn rooted_table(vfs: &mut MemVfs) -> (PartSpec, PublishOutcome, u64) {
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::Committed);
    let mut fxid = 100;
    loop {
        let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
        append_int8_rows(&mut w, vfs, &mut kit, ROWS, |i| Some((fxid * 1000 + i) as i64));
        let out = finish_and_publish(&mut w, vfs, &mut kit, &probe);
        fxid += 1;
        if stats_ref(&out.manifest).expect("stats ref").is_some() {
            return (*w.spec(), out, fxid);
        }
        assert!(fxid < 100 + 3 * LOOSE_MAX as u64, "no stats root after {} publishes", fxid - 100);
    }
}

#[test]
fn compile_ext_and_publish_ext_keep_the_stats_root_and_the_tombs_record_in_either_order() {
    let probe = Probe::new(TxnVerdict::Committed);
    for compile_first in [true, false] {
        for catalog_base in [false, true] {
            let mut vfs = mem_with_dir();
            let (spec, mut last, mut fxid) = rooted_table(&mut vfs);
            let root = stats_ref(&last.manifest).expect("stats ref");
            assert!(root.is_some());
            assert_eq!(tombs(&last.manifest.ext).expect("tombs"), TombsRecord::default(), "no EXT_TOMBS before the edits");
            // a switch (slot 1 current, slot 0 retired), then its fold (slot 0 folded by this step's transaction)
            for step in 0..2 {
                let compile = (step == 0) == compile_first;
                let want = TombsRecord { cur: 1, ret: Some(RetiredTombs { slot: 0, folded: if step == 0 { 0 } else { fxid } }) };
                let what = format!("compile first {compile_first}, catalog base {catalog_base}, step {step} ({})", if compile { "compile_ext" } else { "publish_ext" });
                let mut edit = |base: Option<&Manifest>, b: &mut PlanBuilder<'_>| -> WriteResult<()> {
                    b.ext(with_tombs(base.map_or(&[][..], |m| &m.ext[..]), &want)?);
                    Ok(())
                };
                let base = if catalog_base {
                    BaseStats::Gen(BaseGen { manifest: &last.manifest, pointer: last.pointer, stats: &last.stats, full: None })
                } else {
                    BaseStats::None
                };
                let out = if compile {
                    let rows = [(0u32, 3u16), (0, 7)];
                    publish_compile_ext(&mut vfs, &mut NoWal, DIR, &spec, &[(0, &rows[..])], fxid, &probe, base, &mut edit)
                        .expect("compile_ext")
                        .expect("a compile that adds rows publishes")
                } else {
                    publish_ext(&mut vfs, &mut NoWal, DIR, &spec, fxid, &probe, base, &mut edit).expect("publish_ext")
                };
                assert_eq!(out.gen, last.manifest.header.gen + 1, "{what}");
                assert_eq!(out.runs_written, 0, "no stats run written: the root is carried ({what})");
                assert_eq!(out.base_given, catalog_base, "{what}");
                assert_eq!(out.stats.root, root, "the publisher's state keeps the root ({what})");
                if compile {
                    assert_eq!(out.manifest.parts[0].dv_gen, out.gen, "the compile's DV ({what})");
                }
                let on_disk = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("an effective generation");
                assert_eq!(on_disk.header.gen, out.gen, "{what}");
                for (m, whose) in [(&out.manifest, "outcome"), (&on_disk, "on disk")] {
                    assert_eq!(stats_ref(m).expect("one EXT_STATS record"), root, "EXT_STATS survives ({whose}, {what})");
                    assert_eq!(tombs(&m.ext).expect("one EXT_TOMBS record"), want, "EXT_TOMBS is the edit's ({whose}, {what})");
                }
                last = out;
                fxid += 1;
            }
        }
    }
}
