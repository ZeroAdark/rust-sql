//! Unit slice of the M3-H exit gates (the real-backend legs live in
//! `scripts/pgrc-ddl-e2e.sh` / `scripts/pgrc-crash-durability-e2e.sh`):
//! refusal identity pins, the visibility decision, schema derivation (byval hint law), directory
//! path shape, and the ino-reuse invalidation TWO-TEETH test over the
//! shared part registry.

// ---------------------------------------------------------------------------
// Refusal identity (exactness-sweep C12 posture: 0A000 + stable message)
// ---------------------------------------------------------------------------

#[test]
fn refusal_identity_is_pinned() {
    let e = crate::unsupported("TID scans");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
    assert_eq!(e.message(), "pgrcolumnar does not support TID scans");
}

#[test]
fn trickle_refusal_identity_is_pinned() {
    // The restored M3-H ModifyTable gate's error identity (the M5e scout's
    // F-1; wording frozen to the v3 gate @ dc3c67c56214 so error-parity
    // suites see ONE message shape across the lanes): 0A000 always.
    use crate::dml::{trickle_unsupported, TrickleOp};
    for (op, msg) in [(TrickleOp::Merge, "pgrcolumnar does not support MERGE")] {
        let e = trickle_unsupported(op);
        assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message(), msg);
    }
}

#[test]
fn write_error_sqlstates_are_class_honest() {
    use pgrc_write::WriteError as W;
    assert_eq!(
        crate::write_error(W::Refused { what: "x" }).sqlstate(),
        types_error::ERRCODE_FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        crate::write_error(W::ManifestChain { at: "x" }).sqlstate(),
        types_error::ERRCODE_DATA_CORRUPTED
    );
    assert_eq!(
        crate::write_error(W::Io {
            op: "open",
            path: "p".into(),
            errno: 5
        })
        .sqlstate(),
        types_error::ERRCODE_IO_ERROR
    );
    // errcode_for_file_access's classes, as C's file access errors; a synthesized failure (errno 0) stays an I/O error
    let io = |errno| crate::write_error(W::Io { op: "open_ro", path: "p".into(), errno }).sqlstate();
    assert_eq!(io(libc::EACCES), types_error::ERRCODE_INSUFFICIENT_PRIVILEGE);
    assert_eq!(io(libc::ENOENT), types_error::ERRCODE_UNDEFINED_FILE);
    assert_eq!(io(libc::ENOSPC), types_error::ERRCODE_DISK_FULL);
    assert_eq!(io(0), types_error::ERRCODE_IO_ERROR);
    let msg = crate::write_error(W::Io { op: "open_ro", path: "p".into(), errno: libc::EACCES });
    assert!(msg.message().ends_with(": Permission denied"), "C's %m text: {}", msg.message());
    let rio = |errno| crate::read_error(pgrc_read::ReadError::Io { at: "open", errno }).sqlstate();
    assert_eq!(rio(libc::EACCES), types_error::ERRCODE_INSUFFICIENT_PRIVILEGE);
    assert_eq!(rio(0), types_error::ERRCODE_IO_ERROR);
}

// ---------------------------------------------------------------------------
// Visibility decision (the rg_visible law at manifest grain)
// ---------------------------------------------------------------------------

#[test]
fn visibility_law_is_the_old_am_truth_table() {
    use crate::probe::visible;
    // Own transaction: always visible.
    assert!(visible(true, false, false));
    assert!(visible(true, true, false));
    // Committed and not in snapshot: visible.
    assert!(visible(false, false, true));
    // In snapshot (concurrent at snapshot time): invisible even if now
    // committed.
    assert!(!visible(false, true, true));
    // Uncommitted foreign publisher: invisible.
    assert!(!visible(false, false, false));
}

// ---------------------------------------------------------------------------
// Schema derivation (the byval hint law)
// ---------------------------------------------------------------------------

mod schema_tests {
    use types_tuple::tupdesc::{FormData_pg_attribute, NameData};

    fn att(
        atttypid: u32,
        attlen: i16,
        attbyval: bool,
        attalign: i8,
        attnum: i16,
    ) -> FormData_pg_attribute {
        let mut a = FormData_pg_attribute {
            atttypid,
            attlen,
            attnum,
            attbyval,
            attalign,
            ..Default::default()
        };
        a.attname = NameData { data: [0; 64] };
        a.attname.data[..3].copy_from_slice(b"col");
        a.attname.data[3] = b'0' + (attnum as u8 % 10);
        a
    }

    #[test]
    fn supported_shapes_pass_the_create_gate() {
        let atts = [
            att(23, 4, true, b'i' as i8, 1),     // int4 (signed hint)
            att(700, 4, true, b'i' as i8, 2),    // float4
            att(16, 1, true, b'c' as i8, 3),     // bool
            att(26, 4, true, b'i' as i8, 4),     // oid (unsigned hint)
            att(25, -1, false, b'i' as i8, 5),   // text (varlena)
            att(2950, 16, false, b'c' as i8, 6), // uuid (fixed byref)
        ];
        assert!(crate::schema::check_create_supported("t", &atts).is_ok());
    }

    #[test]
    fn unhinted_byval_type_refuses_at_create() {
        // a byval oid past the hint map with no catalog probe installed (a mis-hinted
        // signedness would corrupt datum words silently); money (790), pg_lsn, xid8 and the
        // reg* words are in the map
        let atts = [att(16499, 4, true, b'i' as i8, 1)];
        let err = crate::schema::check_create_supported("t", &atts).unwrap_err();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert!(err.message().contains("type oid 16499"), "{}", err.message());
        let atts = [att(790, 8, true, b'd' as i8, 1), att(3220, 8, true, b'd' as i8, 2), att(5069, 8, true, b'd' as i8, 3), att(2205, 4, true, b'i' as i8, 4), att(24, 4, true, b'i' as i8, 5)];
        assert!(crate::schema::check_create_supported("t", &atts).is_ok());
    }

    #[test]
    fn cstring_typlen_refuses() {
        let atts = [att(2275, -2, false, b'c' as i8, 1)];
        assert!(crate::schema::check_create_supported("t", &atts).is_err());
    }
}

// ---------------------------------------------------------------------------
// Directory path shape (O-7: sibling of the main fork)
// ---------------------------------------------------------------------------

#[test]
fn table_dir_is_a_sibling_of_the_main_fork() {
    use types_storage::storage::RelFileLocator;
    // -1 = INVALID_PROC_NUMBER (non-temp relation).
    let dir = crate::dirpath::table_dir_path(
        RelFileLocator {
            spcOid: 1663,
            dbOid: 5,
            relNumber: 16384,
        },
        -1,
    );
    assert_eq!(dir, "base/5/pgrc_16384");
}

// ---------------------------------------------------------------------------
// The ino-reuse hole: stale hit without invalidation (tooth 1), correct
// bytes after invalidate_relid (tooth 2) — the M3-F→M3-H seam contract.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The catalog load (read-only, over a directory end-of-recovery cleanup
// left) and the reader walk. Real-FS tempdirs; only the probes are doubles.
// ---------------------------------------------------------------------------

pub(crate) mod catalog_load {
    use pgrc_format::manifest::CommitPointer;
    use pgrc_read::{AllCommitted, TableExpect};
    use pgrc_write::elect::CandidateSource;
    use pgrc_write::ingest::{NoExternalDetoast, RawDatum};
    use pgrc_write::publish::{PublishOutcome, TxnProbe, TxnVerdict};
    use pgrc_write::seal::ReferenceResolver;
    use pgrc_write::shred::NoShred;
    use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter, TxnStamp};
    use pgrc_write::wvfs::{MemVfs, WriteVfs};

    use crate::catalog::{self, Lookup};
    use crate::scan::resolve_for_scan;

    struct CommittedProbe;
    impl TxnProbe for CommittedProbe {
        fn verdict(&self, _fxid: u64) -> TxnVerdict {
            TxnVerdict::Committed
        }
    }

    struct None_;
    impl pgrc_read::CommitCheck for None_ {
        fn committed(&self, _: u64) -> bool {
            false
        }
    }

    fn expect_none() -> TableExpect {
        TableExpect {
            relfilenumber: None,
            schema_fingerprint: None,
        }
    }

    /// A table directory written over MemVfs by the real writer: single-column int8 publishes, one transaction each.
    pub(crate) struct Dir {
        vfs: MemVfs,
        relfilenumber: u64,
    }

    impl Dir {
        pub(crate) fn new(relfilenumber: u64) -> Dir {
            let mut vfs = MemVfs::new();
            vfs.mkdir_path("t").expect("mkdir");
            Dir { vfs, relfilenumber }
        }

        /// One publish of `values` by transaction `fxid` over `base`, a part per `rows_per_part` rows.
        pub(crate) fn publish(&mut self, fxid: u64, values: &[i64], rows_per_part: u64, base: pgrc_write::publish::BaseStats<'_>) -> PublishOutcome {
            self.publish_by(fxid, values, rows_per_part, base, &CommittedProbe)
        }

        /// [`Dir::publish`] under `probe` (GENID: its verdicts and the directory's used numbers).
        pub(crate) fn publish_by(&mut self, fxid: u64, values: &[i64], rows_per_part: u64, base: pgrc_write::publish::BaseStats<'_>, probe: &dyn TxnProbe) -> PublishOutcome {
            use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
            let schema = vec![ColSchema {
                attno: 1,
                class: StorageClass::ByvalWord { width: 8, signed: true },
                typlen: 8,
                typbyval: true,
                typalign: b'd',
                collation_class: CollationClass::C,
                semantics: TypeSemantics::SignedInt,
            }];
            let policy = PartCutPolicy { max_rows: rows_per_part, cut_granule_rows: rows_per_part.min(u64::from(u32::MAX)) as u32, ..PartCutPolicy::default() };
            let policy = if rows_per_part == u64::MAX { PartCutPolicy::default() } else { policy };
            let mut w = TableWriter::open("t".to_string(), schema, 1663, 5, self.relfilenumber, TxnStamp { fxid, cid: 1 }, &SubxactEvidence::default(), policy).expect("open");
            let sources: [&dyn CandidateSource; 0] = [];
            let resolver = ReferenceResolver;
            let mut shred = NoShred;
            let opts = pgrc_format::relopt::ShredOptions::default();
            let mut env = SealEnv { vfs: &mut self.vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
            for &v in values {
                w.append_row(&[RawDatum::Word(v as u64)], &mut NoExternalDetoast, &mut env).expect("append");
            }
            w.finish(&mut env).expect("finish");
            w.publish_with_gen(&mut self.vfs, probe, base).expect("publish")
        }

        fn files(&mut self) -> Vec<(String, Vec<u8>)> {
            let names = self.vfs.list_dir("t").expect("list");
            names.into_iter().map(|n| {
                let bytes = self.vfs.read_full(&format!("t/{n}")).expect("read");
                (n, bytes)
            }).collect()
        }

        /// Every file of the directory into `dir` on disk.
        fn copy_to(&mut self, dir: &str) {
            for (name, bytes) in self.files() {
                std::fs::write(format!("{dir}/{name}"), bytes).expect("write");
            }
        }

        /// GENID: every new or changed file of the directory into `dir` on disk as a publisher installs it, a new
        /// file renamed over the name (a new inode: an open file of the old name keeps the old one); an unchanged file
        /// stays as it is.
        fn install_to(&mut self, dir: &str) {
            for (name, bytes) in self.files() {
                let path = format!("{dir}/{name}");
                if std::fs::read(&path).is_ok_and(|b| b == bytes) {
                    continue;
                }
                let tmp = format!("{dir}/.{name}.tmp");
                std::fs::write(&tmp, bytes).expect("write");
                std::fs::rename(&tmp, &path).expect("rename");
            }
        }
    }

    /// One committed single-column int8 publish (gen 1, fxid 42, value 77)
    /// through the real writer over MemVfs: every (name, bytes) of the directory.
    fn gen1_files(relfilenumber: u64) -> Vec<(String, Vec<u8>)> {
        let mut d = Dir::new(relfilenumber);
        d.publish(42, &[77], u64::MAX, pgrc_write::publish::BaseStats::None);
        d.files()
    }

    fn temp_dir(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("pgrc_am_load_{}_{tag}", std::process::id()));
        let dir = dir.to_str().expect("utf8 tempdir").to_string();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    /// The facts image with the storage widths blanked (a fold over the parts does not know them).
    fn mask(img: &pgrc_format::facts::FactsImage) -> Vec<u8> {
        let mut img = img.clone();
        img.cols.iter_mut().for_each(|c| c.width = 0);
        img.encode().unwrap()
    }

    /// Every part's part-grain record of column 1, as the answers serve it.
    fn records(g: &catalog::Generation) -> Vec<Option<Vec<u8>>> {
        let a = g.answers().expect("answers");
        (0..a.parts()).map(|i| a.part_record(1, i).map(<[u8]>::to_vec)).collect()
    }

    /// The same records built from the open parts (the completion's walk).
    fn walked_records(g: &catalog::Generation) -> Vec<Option<Vec<u8>>> {
        let parts: Vec<&pgrc_read::OpenPart> = g.parts.iter().map(|p| &*p.part).collect();
        let a = crate::answers::Answers::new(parts.len(), crate::answers::build_from_parts(&parts).expect("walk")).expect("answers");
        (0..a.parts()).map(|i| a.part_record(1, i).map(<[u8]>::to_vec)).collect()
    }

    fn committed_dir(tag: &str, relfilenumber: u64) -> String {
        let dir = temp_dir(tag);
        for (name, bytes) in gen1_files(relfilenumber) {
            std::fs::write(format!("{dir}/{name}"), bytes).expect("write");
        }
        dir
    }

    fn snapshot(dir: &str) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
            .expect("list")
            .map(|e| {
                let e = e.expect("entry");
                (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).expect("read"))
            })
            .collect();
        v.sort();
        v
    }

    /// The load reads the generation's stats (P-1: a one-part generation's one loose part, from the open part's
    /// footer; nothing folded), serves the generation from memory, and writes nothing: the directory is
    /// byte-identical after.
    #[test]
    fn the_catalog_loads_a_directory_read_only_and_serves_it_from_memory() {
        let dir = committed_dir("catalog", 777);
        let slot: catalog::RelSlot = (1663, 5, 777);
        catalog::remove_dir(&dir);
        assert!(matches!(catalog::visible(slot, &AllCommitted).unwrap(), Lookup::Unknown));
        let before = snapshot(&dir);
        assert!(!before.iter().any(|(n, _)| n.starts_with("sections-")), "P-1: the publisher writes no sections file");
        let loaded = catalog::load_dir_with(slot, &dir, &CommittedProbe, &|_| panic!("the stats serve"), &|_| Ok(None)).expect("load");
        assert_eq!((loaded.gens, loaded.parts, loaded.complete, loaded.completed, loaded.runs, loaded.loose), (1, 1, 1, 0, 0, 1), "{loaded:?}");
        assert_eq!(snapshot(&dir), before, "a load never writes");
        let Lookup::Gen(g) = catalog::visible(slot, &AllCommitted).unwrap() else { panic!("gen 1 visible") };
        assert_eq!((g.gen, g.prev_gen, g.manifest.parts.len()), (1, 0, 1));
        assert_eq!(g.manifest.parts[0].rows, 1);
        // the open warmed the part's StreamDir: it is among the faults already, and asking for it faults nothing more
        let part = &g.parts[0].part;
        let faults = part.faults();
        assert!(faults.iter().any(|f| matches!(f.tag, pgrc_read::openpart::FaultTag::ListedSection { kind, .. } if kind == pgrc_format::part::SectionKind::StreamDir as u16)), "{faults:?}");
        let _ = part.stream_directory().expect("dir");
        assert_eq!(part.faults().len(), faults.len(), "the directory was resident since the open");
        let answers = g.answers().expect("answers held");
        assert_eq!(answers.parts(), 1);
        assert!(answers.part_record(1, 0).is_some_and(|b| b.len() == pgrc_format::meta::STATS_RECORD_LEN));
        assert!(matches!(catalog::visible(slot, &None_).unwrap(), Lookup::Empty), "no committed publisher: empty, not a walk");
        let image = g.image().expect("image");
        assert_eq!((image.header.rows, image.header.parts, image.cols.len()), (1, 1, 1));
        assert_eq!(mask(&image), mask(&crate::footer::fold_generation(&g).expect("fold")), "the image equals the fold over the parts");
        assert_eq!(records(&g), walked_records(&g));
        catalog::remove_dir(&dir);
        assert!(matches!(catalog::visible(slot, &AllCommitted).unwrap(), Lookup::Unknown));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A load over an entry the catalog already holds (a publish, or an earlier load) merges:
    /// the held generation wins, nothing is duplicated.
    #[test]
    fn a_load_merges_into_the_held_entry() {
        let dir = committed_dir("merge", 778);
        let slot: catalog::RelSlot = (1663, 5, 778);
        catalog::remove_dir(&dir);
        catalog::load_dir_with(slot, &dir, &CommittedProbe, &|_| panic!("the file serves"), &|_| Ok(None)).expect("load");
        let Lookup::Gen(held) = catalog::visible(slot, &AllCommitted).unwrap() else { panic!("gen 1 visible") };
        catalog::load_dir_with(slot, &dir, &CommittedProbe, &|_| panic!("the file serves"), &|_| Ok(None)).expect("load");
        let Lookup::Gen(g) = catalog::visible(slot, &AllCommitted).unwrap() else { panic!("gen 1 visible") };
        assert!(std::sync::Arc::ptr_eq(&held, &g));
        let (gens, head) = catalog::generations(slot).expect("entry");
        assert_eq!((gens.len(), head), (1, 1));
        catalog::remove_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CURRENT` naming a generation with no manifest: the load and the reader walk both refuse
    /// as corruption (cleanup ran before either), never a silent empty table.
    #[test]
    fn a_dangling_current_is_corruption() {
        let dir = committed_dir("dangling", 779);
        let slot: catalog::RelSlot = (1663, 5, 779);
        std::fs::write(format!("{dir}/CURRENT"), CommitPointer::new(2, 96, 0xDEAD_BEEF).encode()).expect("dangle CURRENT");
        let err = catalog::load_dir_with(slot, &dir, &CommittedProbe, &|_| panic!("no generation"), &|_| Ok(None)).err().expect("load refuses");
        assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
        assert!(err.message().contains("manifest generation 2"), "{}", err.message());
        assert!(!catalog::contains(slot));
        let err = resolve_for_scan(&dir, &AllCommitted, &expect_none()).expect_err("reader refuses");
        assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
        assert!(err.message().contains("manifest generation 2 missing"), "{}", err.message());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A visible generation with a part is what the engine bridge serves.
    #[test]
    fn the_bridge_serves_a_generation_with_parts() {
        let dir = committed_dir("bridge", 784);
        let slot: catalog::RelSlot = (1663, 5, 784);
        catalog::remove_dir(&dir);
        catalog::load_dir_with(slot, &dir, &CommittedProbe, &|_| panic!("the file serves"), &|_| Ok(None)).expect("load");
        let found = catalog::visible(slot, &AllCommitted).unwrap();
        let Ok(Some(g)) = crate::scan::bridge_generation(found) else { panic!("gen 1 has a part: served") };
        assert_eq!((g.gen, g.manifest.parts.len()), (1, 1));
        catalog::remove_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe for which `fxid` is in progress and every other transaction committed.
    struct InProgress(u64);
    impl TxnProbe for InProgress {
        fn verdict(&self, fxid: u64) -> TxnVerdict {
            if fxid == self.0 { TxnVerdict::InProgress } else { TxnVerdict::Committed }
        }
    }

    /// SEAL-OFFHEAD: a directory whose head is a stacked generation (a maintenance publish over a bulk writer that
    /// was open when the server stopped) is queued for a settle by its load, so the front launcher's next sweep leaves
    /// a plain head; a plain head queues nothing.
    #[test]
    fn a_load_queues_a_settle_for_a_stacked_head() {
        use pgrc_write::testkit::Probe;
        let slot: catalog::RelSlot = (1663, 5, 791);
        let dir = temp_dir("settle-at-load");
        let mut d = Dir::new(slot.2);
        d.publish(42, &[1], 1, pgrc_write::publish::BaseStats::None);
        let open = Probe::new(TxnVerdict::Committed).set(43, TxnVerdict::InProgress);
        let a = d.publish_by(43, &[2], 1, pgrc_write::publish::BaseStats::None, &open);
        let spec = pgrc_write::seal::PartSpec { spc: 1663, db: 5, relfilenumber: slot.2, schema_fingerprint: a.manifest.header.schema_fingerprint };
        let x = pgrc_write::publish::publish_ext(&mut d.vfs, &mut pgrc_write::walsink::NoWal, "t", &spec, 44, &Probe::new(TxnVerdict::Committed).set(43, TxnVerdict::InProgress).set(44, TxnVerdict::InProgress).stacking(), (None, None), &mut |_, _| Ok(()))
            .expect("a maintenance publish stacks");
        assert_eq!(x.stacked_over, vec![43]);
        d.copy_to(&dir);
        catalog::remove_dir(&dir);
        catalog::settle_due();
        let restarted = Probe::new(TxnVerdict::Committed).set(43, TxnVerdict::Aborted);
        catalog::load_dir_with(slot, &dir, &restarted, &|g| crate::footer::fold_generation(g), &|_| Ok(None)).expect("load");
        assert!(catalog::settle_due().contains(&slot), "the stacked head is queued");

        let plain: catalog::RelSlot = (1663, 5, 792);
        let pdir = temp_dir("settle-at-load-plain");
        let mut p = Dir::new(plain.2);
        p.publish(42, &[1], 1, pgrc_write::publish::BaseStats::None);
        p.copy_to(&pdir);
        catalog::remove_dir(&pdir);
        catalog::load_dir_with(plain, &pdir, &CommittedProbe, &|g| crate::footer::fold_generation(g), &|_| Ok(None)).expect("load");
        assert!(!catalog::settle_due().contains(&plain), "a plain head is not");
    }

    /// P-1 L2: the load reads each generation's stats once and carries them: the runs it reads equal the publisher's,
    /// a later generation takes the earlier one's runs, loose parts and open parts by reference (pointer-equal), and so
    /// do a redone publish and an in-process publish on the catalog's head; the image and the answers equal the ones
    /// built from the parts.
    #[test]
    fn the_load_equals_the_publisher_state_and_generations_share_runs() {
        use std::sync::Arc;
        let slot: catalog::RelSlot = (1663, 5, 781);
        let dir = temp_dir("runs");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &(0..20).collect::<Vec<i64>>(), 1, pgrc_write::publish::BaseStats::None);
        assert!(o1.stats.root.is_some() && o1.stats.runs.len() == 1 && o1.stats.loose.is_empty(), "twenty parts in one install: one run");
        let o2 = d.publish(43, &[100], 1, pgrc_write::publish::BaseStats::None);
        assert_eq!((o2.stats.runs.len(), o2.stats.loose.len()), (1, 1));
        d.copy_to(&dir);
        catalog::remove_dir(&dir);
        // 43 in progress: both generations kept, the second loaded over the first
        let loaded = catalog::load_dir_with(slot, &dir, &InProgress(43), &|_| panic!("the stats serve"), &|_| Ok(None)).expect("load");
        assert_eq!((loaded.gens, loaded.complete, loaded.completed, loaded.runs, loaded.loose), (2, 2, 0, 2, 1), "{loaded:?}");
        let (gens, head) = catalog::generations(slot).expect("entry");
        assert_eq!((gens.len(), head), (2, 2));
        let (g1, g2) = (&gens[0], &gens[1]);
        let (s1, s2) = (g1.stats.as_ref().expect("stats"), g2.stats.as_ref().expect("stats"));
        assert_eq!(s1.runs, o1.stats.runs, "the run read back is the publisher's");
        assert_eq!(s1.root, o1.stats.root);
        let idents = |s: &pgrc_write::stats::StatsState| s.loose.iter().map(|p| p.ident).collect::<Vec<_>>();
        assert_eq!(idents(s1), idents(&o1.stats));
        assert_eq!(idents(s2), idents(&o2.stats));
        assert!(Arc::ptr_eq(&s1.runs[0], &s2.runs[0]), "the second generation carries the first's run");
        assert!(g1.parts.iter().zip(&g2.parts).all(|(a, b)| Arc::ptr_eq(&a.part, &b.part)), "and its open parts");
        for g in [g1, g2] {
            assert_eq!(mask(&g.image().expect("image")), mask(&crate::footer::fold_generation(g).expect("fold")), "gen {}: the image equals the fold", g.gen);
            assert_eq!(records(g), walked_records(g), "gen {}: the answers equal the parts'", g.gen);
        }
        // a standby's redo of the next publish reads its new part only: the run and the parts carried
        let o3 = d.publish(44, &[200], 1, g2.base_hold().stats());
        assert!(o3.base_given, "CURRENT still names the catalog's head: the publisher read no manifest or run");
        d.copy_to(&dir);
        catalog::redo_publish(slot, &dir, o3.manifest.clone(), &|_| panic!("the stats serve"), &|_| Ok(None)).expect("redo");
        let (gens, _) = catalog::generations(slot).expect("entry");
        let g3 = Arc::clone(&gens[2]);
        let s3 = g3.stats.as_ref().expect("stats");
        assert!(Arc::ptr_eq(&s1.runs[0], &s3.runs[0]), "redo carries the run");
        assert!(Arc::ptr_eq(&s2.loose[0], &s3.loose[0]), "and the loose part's stats (not read again)");
        assert!(g2.parts.iter().zip(&g3.parts).all(|(a, b)| Arc::ptr_eq(&a.part, &b.part)));
        assert_eq!(idents(s3), idents(&o3.stats));
        // an in-process publish on the head: the publisher took the head's runs by reference, so does the catalog
        let o4 = d.publish(45, &[300], 1, g3.base_hold().stats());
        assert!(o4.base_given);
        d.copy_to(&dir);
        let before = snapshot(&dir);
        let g4 = catalog::publish(slot, &dir, o4.manifest.clone(), o4.stats.clone(), &|_| panic!("the stats serve")).expect("publish");
        assert_eq!(snapshot(&dir), before, "the catalog's publish writes nothing");
        let s4 = g4.stats.as_ref().expect("stats");
        assert!(Arc::ptr_eq(&s1.runs[0], &s4.runs[0]), "the publish carries the run");
        assert!(s3.loose.iter().zip(&s4.loose).all(|(a, b)| Arc::ptr_eq(a, b)) && s4.loose.len() == 3, "and the loose parts' stats");
        assert!(g3.parts.iter().zip(&g4.parts).all(|(a, b)| Arc::ptr_eq(&a.part, &b.part)));
        assert_eq!(**s4, o4.stats);
        assert_eq!(mask(&g4.image().expect("image")), mask(&crate::footer::fold_generation(&g4).expect("fold")));
        assert_eq!(records(&g4), walked_records(&g4));
        catalog::remove_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A root that does not describe its generation (a run file's bytes corrupted) is never trusted: the load logs it
    /// and completes the generation from its parts in memory, and writes nothing.
    #[test]
    fn a_corrupt_run_falls_back_to_the_parts() {
        let slot: catalog::RelSlot = (1663, 5, 782);
        let dir = temp_dir("corrupt");
        let mut d = Dir::new(slot.2);
        d.publish(42, &(0..20).collect::<Vec<i64>>(), 1, pgrc_write::publish::BaseStats::None);
        d.copy_to(&dir);
        let runs: Vec<String> = snapshot(&dir).into_iter().map(|(n, _)| n).filter(|n| n.starts_with("srun-")).collect();
        assert!(!runs.is_empty());
        for n in &runs {
            let p = format!("{dir}/{n}");
            let mut b = std::fs::read(&p).expect("run");
            let at = b.len() / 2;
            b[at] ^= 0x5a;
            std::fs::write(&p, b).expect("corrupt");
        }
        catalog::remove_dir(&dir);
        let before = snapshot(&dir);
        let loaded = catalog::load_dir_with(slot, &dir, &CommittedProbe, &|g| crate::footer::fold_generation(g), &|_| Ok(None)).expect("load");
        assert_eq!((loaded.gens, loaded.complete, loaded.completed, loaded.runs), (1, 0, 1, 0), "{loaded:?}");
        assert_eq!(snapshot(&dir), before, "a load never writes");
        let Lookup::Gen(g) = catalog::visible(slot, &AllCommitted).unwrap() else { panic!("gen 1 visible") };
        assert!(g.stats.is_none(), "the refused stats are not held");
        assert_eq!(g.image().expect("image").header.rows, 20);
        assert_eq!(records(&g), walked_records(&g));
        catalog::remove_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A generation written before P-1 without its sections file, of `LOOSE_MAX` parts or more and no root: completed
    /// in memory from its parts (the one-time walk), nothing written; the next publish on it takes its image and
    /// answers as the seed of its legacy run.
    #[test]
    fn a_rootless_generation_of_many_parts_is_completed_in_memory() {
        let slot: catalog::RelSlot = (1663, 5, 783);
        let dir = temp_dir("rootless");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &(0..20).collect::<Vec<i64>>(), 1, pgrc_write::publish::BaseStats::None);
        d.copy_to(&dir);
        // the directory as the base binary left it: the manifest without its EXT_STATS record, no run file
        let m = o1.manifest.clone().with_ext(pgrc_format::manifest::with_stats_ref(&o1.manifest.ext, None).expect("ext"));
        let bytes = m.encode();
        std::fs::write(format!("{dir}/{}", pgrc_format::dirlayout::manifest_file_name(1)), &bytes).expect("manifest");
        std::fs::write(format!("{dir}/CURRENT"), pgrc_write::publish::commit_pointer(&m).encode()).expect("CURRENT");
        for (n, _) in snapshot(&dir).into_iter().filter(|(n, _)| n.starts_with("srun-")) {
            std::fs::remove_file(format!("{dir}/{n}")).expect("unlink run");
        }
        catalog::remove_dir(&dir);
        let before = snapshot(&dir);
        let loaded = catalog::load_dir_with(slot, &dir, &CommittedProbe, &|g| crate::footer::fold_generation(g), &|_| Ok(None)).expect("load");
        assert_eq!((loaded.gens, loaded.parts, loaded.complete, loaded.completed), (1, 20, 0, 1), "{loaded:?}");
        assert_eq!(snapshot(&dir), before, "a load never writes");
        let Lookup::Gen(g) = catalog::visible(slot, &AllCommitted).unwrap() else { panic!("gen 1 visible") };
        assert!(g.stats.is_none());
        assert_eq!(records(&g), walked_records(&g));
        assert!(matches!(g.base_hold().stats(), pgrc_write::publish::BaseStats::Folded(Some(_), Some(_))), "the seed of the legacy run");
        catalog::remove_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reader_walk_resolves_the_committed_generation() {
        let dir = committed_dir("reader", 780);
        let eff = resolve_for_scan(&dir, &AllCommitted, &expect_none()).expect("open").expect("gen 1 is effective");
        assert_eq!((eff.manifest.header.gen, eff.manifest.parts.len(), eff.manifest.parts[0].rows), (1, 1, 1));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(resolve_for_scan(&dir, &AllCommitted, &expect_none()).expect("absent directory").is_none());
    }
    // -----------------------------------------------------------------------
    // GENID (pgrc-plans/GENID.done): numbers never repeat in a directory within the process, a rolled-back
    // publish's generation leaves the entry, and a part is carried only from the generation a publish built on
    // (D1133), checked by identity where a `stat` is cheap.
    // -----------------------------------------------------------------------

    /// A probe for which the `aborted` publishers aborted, the `running` ones are in progress and every other one
    /// committed, with the directory's used numbers.
    struct Genid {
        aborted: Vec<u64>,
        running: Vec<u64>,
        used: (u64, u32),
    }
    impl TxnProbe for Genid {
        fn verdict(&self, fxid: u64) -> TxnVerdict {
            if self.aborted.contains(&fxid) {
                TxnVerdict::Aborted
            } else if self.running.contains(&fxid) {
                TxnVerdict::InProgress
            } else {
                TxnVerdict::Committed
            }
        }
        fn used_numbers(&self, _dir: &str) -> (u64, u32) {
            self.used
        }
    }

    fn no_fold(_: &catalog::Generation) -> types_error::PgResult<pgrc_format::facts::FactsImage> {
        panic!("the stats serve")
    }

    fn gens_of(slot: catalog::RelSlot) -> Vec<u64> {
        catalog::generations(slot).map_or_else(Vec::new, |(g, _)| g.iter().map(|g| g.gen).collect())
    }

    /// A rolled-back publish (gen 2, part 1): the next publish builds on gen 1 and numbers above both (gen 3, part 2);
    /// the dead generation leaves the entry at that publish, and at its redo on a standby; a load never picks it (its
    /// manifest is still in the directory) and learns the numbers again after the process forgot them.
    #[test]
    fn genid_a_rolled_back_publish_s_numbers_are_never_reused_and_it_leaves_the_entry() {
        use std::sync::Arc;
        let slot: catalog::RelSlot = (1663, 5, 7_901);
        let dir = temp_dir("genid_dead");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &[1], 1, pgrc_write::publish::BaseStats::None);
        d.install_to(&dir);
        let g1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish 1");
        assert_eq!(catalog::used_numbers(&dir), (1, 1));
        let o2 = d.publish(43, &[2], 1, g1.base_hold().stats());
        assert_eq!((o2.gen, o2.part_nos.clone()), (2, vec![1]));
        d.install_to(&dir);
        catalog::publish(slot, &dir, o2.manifest.clone(), o2.stats.clone(), &no_fold).expect("publish 2");
        assert_eq!(catalog::used_numbers(&dir), (2, 2));
        // 43 rolls back: 44 builds on gen 1 above the used numbers
        let probe = Genid { aborted: vec![43], running: vec![], used: catalog::used_numbers(&dir) };
        let o3 = d.publish_by(44, &[3], 1, pgrc_write::publish::BaseStats::None, &probe);
        assert_eq!((o3.gen, o3.manifest.header.prev_gen, o3.part_nos.clone()), (3, 1, vec![2]));
        d.install_to(&dir);
        let g3 = catalog::publish(slot, &dir, o3.manifest.clone(), o3.stats.clone(), &no_fold).expect("publish 3");
        assert_eq!(gens_of(slot), vec![1, 3], "the dead generation left the entry");
        assert_eq!(catalog::head(slot), Some(3));
        assert!(Arc::ptr_eq(&g1.parts[0].part, &g3.parts[0].part), "part 0 carried from gen 1");
        assert_eq!(super::ino_reuse::decode_single(&g3.parts[1].part), 3);
        assert_eq!(catalog::used_numbers(&dir), (3, 3));
        // a standby: the same drop at the redo
        catalog::remove_dir(&dir);
        let r1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish 1");
        catalog::publish(slot, &dir, o2.manifest.clone(), o2.stats.clone(), &no_fold).expect("publish 2");
        catalog::redo_publish(slot, &dir, o3.manifest.clone(), &no_fold, &|_| Ok(None)).expect("redo 3");
        assert_eq!(gens_of(slot), vec![1, 3], "the redo drops the dead generation too");
        let (gens, _) = catalog::generations(slot).expect("entry");
        assert!(Arc::ptr_eq(&r1.parts[0].part, &gens[1].parts[0].part), "the redo carries part 0 from gen 1");
        // a load: CURRENT names gen 3 (44 in progress), its chain is 3 -> 1; manifest-2 is never read into the entry
        let names: Vec<String> = std::fs::read_dir(&dir).expect("list").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n == "manifest-2.pgrcm"), "the dead manifest is residue until a reclaim: {names:?}");
        catalog::remove_dir(&dir);
        catalog::forget_dir(&dir);
        assert_eq!(catalog::used_numbers(&dir), (0, 0));
        let running = Genid { aborted: vec![43], running: vec![44], used: (0, 0) };
        catalog::load_dir_with(slot, &dir, &running, &no_fold, &|_| Ok(None)).expect("load");
        assert_eq!(gens_of(slot), vec![1, 3], "a load never picks the dead generation");
        assert_eq!(catalog::used_numbers(&dir), (3, 3), "the load learns the numbers again");
        catalog::remove_dir(&dir);
        catalog::forget_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The used numbers survive the directory's contents (a same-transaction TRUNCATE, its redo, an unlogged reset:
    /// the next publish numbers its generation and its parts above them), and go with the directory itself.
    #[test]
    fn genid_used_numbers_survive_an_emptied_directory_and_go_with_a_removed_one() {
        let slot: catalog::RelSlot = (1663, 5, 7_902);
        let dir = temp_dir("genid_used");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &[1, 2], 1, pgrc_write::publish::BaseStats::None);
        assert_eq!((o1.gen, o1.part_nos.clone()), (1, vec![0, 1]));
        d.install_to(&dir);
        let g1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish");
        assert_eq!(catalog::used_numbers(&dir), (1, 2), "a publish");
        let o2 = d.publish(43, &[3], 1, g1.base_hold().stats());
        d.install_to(&dir);
        catalog::redo_publish(slot, &dir, o2.manifest.clone(), &no_fold, &|_| Ok(None)).expect("redo");
        assert_eq!(catalog::used_numbers(&dir), (2, 3), "a redone publish");
        crate::dirpath::remove_dir_contents(&dir).expect("empty");
        assert!(!catalog::contains(slot));
        assert_eq!(std::fs::read_dir(&dir).expect("list").count(), 0);
        assert_eq!(catalog::used_numbers(&dir), (2, 3), "an emptied directory keeps its numbers");
        catalog::create(slot, &dir);
        catalog::redo_truncate(slot, &dir);
        assert_eq!(catalog::used_numbers(&dir), (2, 3), "and a truncate's redo");
        // the first publish after the TRUNCATE: no base, above the used numbers
        let mut t = Dir::new(slot.2);
        let probe = Genid { aborted: vec![], running: vec![], used: catalog::used_numbers(&dir) };
        let o3 = t.publish_by(44, &[9], 1, pgrc_write::publish::BaseStats::None, &probe);
        assert_eq!((o3.gen, o3.manifest.header.prev_gen, o3.part_nos.clone(), o3.manifest.header.next_part_no), (3, 0, vec![3], 4));
        t.install_to(&dir);
        catalog::publish(slot, &dir, o3.manifest.clone(), o3.stats.clone(), &no_fold).expect("publish");
        assert_eq!(catalog::used_numbers(&dir), (3, 4));
        crate::dirpath::remove_table_dir(&dir).expect("remove");
        assert!(!catalog::contains(slot));
        assert_eq!(catalog::used_numbers(&dir), (0, 0), "a removed directory forgets them");
    }

    /// D1133: a rolled-back gen 2 installed part 1; a successor numbered as before GENID (gen 2 again on gen 1, part 1
    /// again, the same length) renamed its own part 1 over the name. Trunk carried from the newest held generation (the
    /// dead one) and so served the dead part's unlinked file. The guard refuses that carry (not the generation the
    /// manifest built on), and an impostor base with the right number refuses it too (part 1 is not below its
    /// `next_part_no`): the part opens again and reads the new rows.
    #[test]
    fn genid_d1133_a_part_is_never_carried_from_a_rolled_back_generation() {
        use std::sync::Arc;
        let slot: catalog::RelSlot = (1663, 5, 7_903);
        let dir = temp_dir("genid_d1133");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &[1], 1, pgrc_write::publish::BaseStats::None);
        d.install_to(&dir);
        let g1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish 1");
        let o2 = d.publish(43, &[2], 1, g1.base_hold().stats());
        d.install_to(&dir);
        let dead = catalog::publish(slot, &dir, o2.manifest.clone(), o2.stats.clone(), &no_fold).expect("publish 2");
        assert_eq!(super::ino_reuse::decode_single(&dead.parts[1].part), 2);
        let old = Genid { aborted: vec![43], running: vec![], used: (0, 0) };
        let o3 = d.publish_by(44, &[3], 1, pgrc_write::publish::BaseStats::None, &old);
        let (a, b) = (o2.manifest.parts[1], o3.manifest.parts[1]);
        assert_eq!((o3.gen, o3.manifest.header.prev_gen, b.part_no, b.file_len, b.footer_off), (2, 1, 1, a.file_len, a.footer_off), "the pre-GENID numbering repeats the dead part's record");
        d.install_to(&dir);
        let stale = Arc::as_ptr(&dead.parts[1].part);
        for verify in [false, true] {
            let parts = catalog::open_parts(slot, &dir, &o3.manifest, Some(&*dead), verify).expect("open");
            assert!(!std::ptr::eq(Arc::as_ptr(&parts[1].part), stale), "the dead generation is not the base (verify {verify})");
            assert_eq!(super::ino_reuse::decode_single(&parts[1].part), 3);
        }
        let mut m = o2.manifest.clone();
        m.header.gen = 1;
        m.header.prev_gen = 0;
        m.header.next_part_no = 1;
        let impostor = dead.with_manifest(m).expect("impostor");
        let parts = catalog::open_parts(slot, &dir, &o3.manifest, Some(&impostor), false).expect("open");
        assert!(Arc::ptr_eq(&parts[0].part, &g1.parts[0].part), "part 0 is below the base's counter: carried");
        assert!(!std::ptr::eq(Arc::as_ptr(&parts[1].part), stale), "part 1 is not below it: opened again");
        assert_eq!(super::ino_reuse::decode_single(&parts[1].part), 3);
        // the catalog's own publish carries from gen 1 (`base_of`)
        let g = catalog::publish(slot, &dir, o3.manifest.clone(), o3.stats.clone(), &no_fold).expect("publish 2 again");
        assert_eq!(super::ino_reuse::decode_single(&g.parts[1].part), 3);
        catalog::remove_dir(&dir);
        catalog::forget_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The identity check: a carried part whose name now names another file (another inode) opens again when
    /// `verify`; a name `stat` cannot see (unlinked, not replaced) is no mismatch; without `verify` the carry is by
    /// record alone.
    #[test]
    fn genid_a_carried_part_whose_name_names_another_file_opens_again() {
        use std::sync::Arc;
        let slot: catalog::RelSlot = (1663, 5, 7_904);
        let dir = temp_dir("genid_ident");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &[1], 1, pgrc_write::publish::BaseStats::None);
        d.install_to(&dir);
        let g1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish 1");
        let o2 = d.publish(43, &[2], 1, g1.base_hold().stats());
        d.install_to(&dir);
        let held = Arc::as_ptr(&g1.parts[0].part);
        let carried = |verify| Arc::as_ptr(&catalog::open_parts(slot, &dir, &o2.manifest, Some(&*g1), verify).expect("open")[0].part) == held;
        assert!(carried(true), "the name still names the held file");
        let part0 = format!("{dir}/part-0.pgrc");
        let bytes = std::fs::read(&part0).expect("read");
        std::fs::write(format!("{part0}.tmp"), &bytes).expect("write");
        std::fs::rename(format!("{part0}.tmp"), &part0).expect("rename");
        assert!(carried(false), "no stat: carried by its record");
        assert!(!carried(true), "another inode under the name: opened again");
        std::fs::remove_file(&part0).expect("unlink");
        assert!(carried(true), "unlinked, not replaced: no mismatch");
        catalog::remove_dir(&dir);
        catalog::forget_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// GENID-2: a publish that drops a rolled-back generation keeps that generation's own parts (not the ones it
    /// carried from its base) for the next reclaim; the reclaim's forget, or a replayed RECLAIM's names, take them off.
    #[test]
    fn genid2_a_dropped_generation_s_own_parts_are_kept_for_the_reclaim() {
        let slot: catalog::RelSlot = (1663, 5, 7_905);
        let dir = temp_dir("genid2_dead");
        let mut d = Dir::new(slot.2);
        let o1 = d.publish(42, &[1, 2], 1, pgrc_write::publish::BaseStats::None);
        d.install_to(&dir);
        let g1 = catalog::publish(slot, &dir, o1.manifest.clone(), o1.stats.clone(), &no_fold).expect("publish 1");
        let o2 = d.publish(43, &[3, 4], 1, g1.base_hold().stats());
        assert_eq!(o2.part_nos, vec![2, 3]);
        d.install_to(&dir);
        catalog::publish(slot, &dir, o2.manifest.clone(), o2.stats.clone(), &no_fold).expect("publish 2");
        assert!(catalog::dead_parts(slot).is_empty(), "nothing dropped yet");
        let probe = Genid { aborted: vec![43], running: vec![], used: catalog::used_numbers(&dir) };
        let o3 = d.publish_by(44, &[5], 1, pgrc_write::publish::BaseStats::None, &probe);
        assert_eq!((o3.gen, o3.part_nos.clone()), (3, vec![4]));
        d.install_to(&dir);
        catalog::publish(slot, &dir, o3.manifest.clone(), o3.stats.clone(), &no_fold).expect("publish 3");
        assert_eq!(catalog::dead_parts(slot), vec![2, 3], "gen 2's own parts; 0 and 1 are gen 1's");
        catalog::forget_dead_parts(slot, &[2]);
        assert_eq!(catalog::dead_parts(slot), vec![3]);
        catalog::forget_dead_names(slot, &["part-3.pgrc".to_string(), "manifest-2.pgrcm".to_string()]);
        assert!(catalog::dead_parts(slot).is_empty());
        catalog::remove_dir(&dir);
        catalog::forget_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

pub(crate) mod ino_reuse {
    use pgrc_format::abi::{ByteArena, DecodeOut};
    use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
    use pgrc_read::registry::PartKey;
    use pgrc_read::{reference_binding_leaked, MemPartIo, PartExpect, StreamCursor};
    use pgrc_write::elect::CandidateSource;
    use pgrc_write::ingest::{NoExternalDetoast, RawDatum};
    use pgrc_write::publish::{TxnProbe, TxnVerdict};
    use pgrc_write::seal::ReferenceResolver;
    use pgrc_write::shred::NoShred;
    use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter, TxnStamp};
    use pgrc_write::wvfs::{MemVfs, WriteVfs};
    use std::sync::Arc;

    /// Distinctive fake device id: parallel tests share the process-global
    /// registry, so this test's identities must be collision-free.
    const DEV: u64 = 0x4D33_4801_0000_0001;
    const INO: u64 = 4242;
    const RELID: u32 = 990_001;

    struct AllCommitted;
    impl TxnProbe for AllCommitted {
        fn verdict(&self, _fxid: u64) -> TxnVerdict {
            TxnVerdict::Committed
        }
    }

    /// Seal one single-column int8 part holding exactly `value`; return the
    /// published part-file bytes.
    pub(crate) fn part_bytes(value: i64) -> Vec<u8> {
        let schema = vec![ColSchema {
            attno: 1,
            class: StorageClass::ByvalWord {
                width: 8,
                signed: true,
            },
            typlen: 8,
            typbyval: true,
            typalign: b'd',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::SignedInt,
        }];
        let mut vfs = MemVfs::new();
        vfs.mkdir_path("t").expect("mkdir");
        let mut w = TableWriter::open(
            "t".to_string(),
            schema,
            1663,
            5,
            777,
            TxnStamp { fxid: 42, cid: 1 },
            &SubxactEvidence::default(),
            PartCutPolicy::default(),
        )
        .expect("open");
        let sources: [&dyn CandidateSource; 0] = [];
        let resolver = ReferenceResolver;
        let mut shred = NoShred;
        let opts = pgrc_format::relopt::ShredOptions::default();
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &resolver,
            shred: &mut shred,
            shred_opts: &opts,
        };
        w.append_row(
            &[RawDatum::Word(value as u64)],
            &mut NoExternalDetoast,
            &mut env,
        )
        .expect("append");
        w.finish(&mut env).expect("finish");
        let out = w.publish(&mut vfs, &AllCommitted).expect("publish");
        vfs.read_full(&format!(
            "t/{}",
            pgrc_format::dirlayout::part_file_name(out.part_nos[0])
        ))
        .expect("part bytes")
    }

    pub(crate) fn decode_single(part: &Arc<pgrc_read::OpenPart>) -> i64 {
        let mut cursor =
            StreamCursor::open(Arc::clone(part), reference_binding_leaked(), 1, 0).expect("cursor");
        let mut datums = [0u64; 1];
        let mut arena = [0u64; 16];
        // SAFETY: u64 → u8 reinterpret of a local buffer, alignment 8.
        let arena_bytes = unsafe {
            core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, arena.len() * 8)
        };
        let mut out = DecodeOut {
            datums: &mut datums,
            arena: ByteArena::new(arena_bytes),
        };
        let rows = cursor.decode_full(0, &mut out).expect("decode");
        assert_eq!(rows, 1);
        datums[0] as i64
    }

    #[test]
    fn ino_reuse_stale_hit_then_invalidate() {
        let a = part_bytes(11);
        let b = part_bytes(22);
        assert_eq!(a.len(), b.len(), "equal-length different-content parts");
        assert_ne!(a, b);

        let reg = crate::inval::registry();

        // Open A under (DEV, INO, len); record its key like a scan does.
        let pin_a = reg
            .open_pinned(&PartExpect::none(), || {
                Ok(Box::new(MemPartIo::new(a.clone(), DEV, INO)) as Box<dyn pgrc_read::PartIo>)
            })
            .expect("open a");
        let ident = pin_a.part().ident();
        let key: PartKey = (ident.dev, ident.ino, ident.len);
        crate::inval::record_part_key(RELID, key);
        assert_eq!(decode_single(pin_a.part()), 11);
        drop(pin_a);

        // TOOTH 1 (the hole is real): the same (dev, ino, len) now carries
        // B's bytes — without invalidation the registry serves A's stale
        // content.
        let pin_stale = reg
            .open_pinned(&PartExpect::none(), || {
                Ok(Box::new(MemPartIo::new(b.clone(), DEV, INO)) as Box<dyn pgrc_read::PartIo>)
            })
            .expect("open stale");
        assert_eq!(
            decode_single(pin_stale.part()),
            11,
            "expected the STALE hit — if this fails the hole closed underneath \
             us and this test must be rewritten, not deleted"
        );
        drop(pin_stale);

        // TOOTH 2 (the seam closes it): relcache-style invalidation drops
        // the recorded keys; the next open reads B's bytes.
        crate::inval::invalidate_relid(RELID);
        let pin_fresh = reg
            .open_pinned(&PartExpect::none(), || {
                Ok(Box::new(MemPartIo::new(b.clone(), DEV, INO)) as Box<dyn pgrc_read::PartIo>)
            })
            .expect("open fresh");
        assert_eq!(decode_single(pin_fresh.part()), 22);
    }
}

/// Tool, not a test (`--ignored`): `DICTCAP_PARTS=<dir>` — one TSV line per
/// stream of every `part-*.pgrc` in the directory: part, attno, path_ord,
/// role, encoding, wrapper, values, extents, disk bytes (the extent lengths),
/// image bytes (the pre-wrapper payload: `uncompressed_len` when wrapped).
/// Feeds `docs/gravity/dictcap.md`.
#[test]
#[ignore]
fn dictcap_census() {
    use pgrc_format::part::{StreamSectionHdr, STREAM_SECTION_HDR_LEN};
    use pgrc_read::{MemPartIo, OpenPart, PartExpect};
    let Ok(dir) = std::env::var("DICTCAP_PARTS") else { return };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pgrc"))
        .collect();
    files.sort();
    for (i, path) in files.iter().enumerate() {
        let bytes = std::fs::read(path).expect("part file");
        let part = OpenPart::open(
            Box::new(MemPartIo::new(bytes.clone(), 7, 1000 + i as u64)),
            &PartExpect::none(),
        )
        .expect("open part");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let sd = part.stream_directory().expect("stream directory");
        for s in sd.streams() {
            let mut disk = 0u64;
            let mut image = 0u64;
            for rec in &s.extents {
                let sec = &bytes[rec.file_off as usize..(rec.file_off + rec.len) as usize];
                let hdr = StreamSectionHdr::decode(sec).expect("section header");
                disk += rec.len;
                image += if hdr.wrapper != 0 {
                    hdr.uncompressed_len as u64
                } else {
                    let end = if hdr.frame_table_off != 0 {
                        hdr.frame_table_off as u64
                    } else if hdr.gcount_table_off != 0 {
                        hdr.gcount_table_off as u64
                    } else {
                        rec.len
                    };
                    end - STREAM_SECTION_HDR_LEN as u64
                };
            }
            println!(
                "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{disk}\t{image}",
                s.entry.attno,
                s.entry.path_ord,
                s.role as u8,
                s.entry.encoding,
                s.entry.wrapper,
                s.entry.values,
                s.extents.len()
            );
        }
    }
}

/// The dictionary NDV cap in force (docs/gravity/dictcap.md §3): 2^24 by
/// default — the widest packed code width, above every part's row count —
/// and the env override parses or falls back. Unit tests have no GUC seam,
/// so this pins the env arm and the constant.
#[test]
fn dict_ndv_cap_default_is_the_code_width_and_env_overrides() {
    const K: &str = "PGRUST_PGRC_DICT_NDV_CAP";
    let saved = std::env::var(K).ok();
    std::env::remove_var(K);
    assert_eq!(crate::ingest::dict_ndv_cap(), 1 << 24);
    std::env::set_var(K, " 100000 ");
    assert_eq!(crate::ingest::dict_ndv_cap(), 100_000);
    std::env::set_var(K, "not-a-number");
    assert_eq!(crate::ingest::dict_ndv_cap(), 1 << 24);
    match saved {
        Some(v) => std::env::set_var(K, v),
        None => std::env::remove_var(K),
    }
}

/// The planner's `sorted_by` claim over one part (`reloptions::attested_prefix`): only the prefix of
/// the declared key the part's SortKey section vouches for, ASC NULLS LAST. A front segment's seal
/// writes `nkeys = 0` (heap order), so a declared key over such a part claims nothing.
#[test]
fn sorted_by_claim_is_the_attested_prefix() {
    use crate::reloptions::attested_prefix;
    use pgrc_format::sortkey::{NullsOrder, SortDir, SortKeyEntry};
    let e = |attno: u32, dir: SortDir, nulls: NullsOrder| SortKeyEntry { attno, dir: dir as u8, nulls: nulls as u8, collation_class: 0, pad: 0 };
    let asc = |attno: u32| e(attno, SortDir::Asc, NullsOrder::Last);
    // unordered part (a front seal, or an undeclared writer)
    assert_eq!(attested_prefix(&[1], &[]), 0);
    assert_eq!(attested_prefix(&[1, 2], &[]), 0);
    // the writer's own key
    assert_eq!(attested_prefix(&[1], &[asc(1)]), 1);
    assert_eq!(attested_prefix(&[1, 3], &[asc(1), asc(3)]), 2);
    // a longer attested key orders every prefix of it; a shorter one only its own length
    assert_eq!(attested_prefix(&[1], &[asc(1), asc(2)]), 1);
    assert_eq!(attested_prefix(&[1, 2], &[asc(1)]), 1);
    // another column, another position, another direction or nulls placement
    assert_eq!(attested_prefix(&[1], &[asc(2)]), 0);
    assert_eq!(attested_prefix(&[1, 2], &[asc(2), asc(1)]), 0);
    assert_eq!(attested_prefix(&[1, 2], &[asc(1), asc(3)]), 1);
    assert_eq!(attested_prefix(&[1], &[e(1, SortDir::Desc, NullsOrder::Last)]), 0);
    assert_eq!(attested_prefix(&[1], &[e(1, SortDir::Asc, NullsOrder::First)]), 0);
    assert_eq!(attested_prefix(&[1, 2], &[asc(1), e(2, SortDir::Desc, NullsOrder::First)]), 1);
}

#[test]
fn the_flush_pass_reaches_every_file_before_the_first_fsync() {
    use std::cell::RefCell;
    let paths: Vec<String> = (0..5).map(|i| format!("d/part-{i}.pgrc")).collect();
    let log = RefCell::new(Vec::new());
    let r: Result<(), &str> = crate::dirpath::flush_then_fsync(
        &paths,
        |p| log.borrow_mut().push(format!("flush {p}")),
        |p| {
            log.borrow_mut().push(format!("fsync {p}"));
            Ok(())
        },
    );
    assert_eq!(r, Ok(()));
    let want: Vec<String> =
        paths.iter().map(|p| format!("flush {p}")).chain(paths.iter().map(|p| format!("fsync {p}"))).collect();
    assert_eq!(*log.borrow(), want);
    log.borrow_mut().clear();
    let r = crate::dirpath::flush_then_fsync(
        &paths,
        |p| log.borrow_mut().push(format!("flush {p}")),
        |p| {
            log.borrow_mut().push(format!("fsync {p}"));
            if p.ends_with("part-2.pgrc") { Err("EIO") } else { Ok(()) }
        },
    );
    assert_eq!(r, Err("EIO"), "an fsync error ends the walk and is returned");
    let fsyncs: Vec<String> = log.borrow().iter().filter(|l| l.starts_with("fsync")).cloned().collect();
    assert_eq!(fsyncs, ["fsync d/part-0.pgrc", "fsync d/part-1.pgrc", "fsync d/part-2.pgrc"]);
    assert_eq!(log.borrow().iter().filter(|l| l.starts_with("flush")).count(), 5);
}

#[test]
fn sync_table_dir_syncs_real_files_and_skips_an_absent_directory() {
    let dir = std::env::temp_dir().join(format!("pgrc_am_sync_{}", std::process::id()));
    let dir = dir.to_str().expect("utf8 tempdir").to_string();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tempdir");
    for i in 0..3 {
        std::fs::write(format!("{dir}/part-{i}.pgrc"), b"pgrc").expect("write");
    }
    crate::dirpath::sync_table_dir(&dir).expect("sync");
    crate::dirpath::flush_path(&format!("{dir}/absent"));
    crate::dirpath::sync_table_dir(&format!("{dir}/absent")).expect("an absent directory has nothing to sync");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

mod rowmap_bench;
mod sortruns;

/// The product binding carries the integer pre-pass unwrapper (id 8), and both of its faces rebuild a pre-passed
/// section byte-exactly.
#[test]
fn product_binding_unwraps_intpre_sections() {
    use pgrc_format::enc::Wrapper;
    use pgrc_format::part::{StreamSectionHdr, StreamSectionWriter};
    let binding = crate::scan::codec_binding();
    let uw = binding.unwrappers.iter().find(|u| u.wrapper() == Wrapper::IntPre).expect("IntPre unwrapper registered");
    let mut sec = Vec::new();
    let mut ends = Vec::new();
    let mut w = StreamSectionWriter::begin(&mut sec, 0, 8, Wrapper::None).expect("begin");
    let mut ts = 1_372_708_800_000_000u64;
    for _ in 0..2 {
        for i in 0..8192u64 {
            ts += (i % 7) * 1_000_000;
            w.payload().extend_from_slice(&ts.to_le_bytes());
        }
        w.end_granule(8192);
        ends.push(w.payload_off());
    }
    w.finish(false).expect("finish");
    let mut wrapped = Vec::new();
    pgrc_codec::wrapper::wrap_section(&sec, &ends, Wrapper::IntPre, &mut wrapped).expect("wrap");
    let hdr = StreamSectionHdr::decode(&wrapped).expect("hdr");
    assert_eq!(hdr.wrapper, Wrapper::IntPre.as_u8());
    assert!(wrapped.len() * 4 < sec.len(), "the pre-pass must engage on second-granular timestamps");
    assert_eq!(uw.unwrap_section(&hdr, &wrapped).expect("unwrap"), sec);
    assert_eq!(uw.unwrap_section_until(&hdr, &wrapped, &mut || false).expect("unwrap until"), Some(sec));
}
