//! D509 (M3-MERGE): a MergeAppend that prunes its arms (partition_prune-089: a generic plan's initial pruning over three
//! index arms) declares src.prune over its arm pipelines, one run covering every arm (exec v4/m2.rs `Prune` sets the skip
//! words and hands a pruned arm's pipeline an empty claim source), and each arm's function reads its own skip word: the
//! Feed step's gate (runs/mergeappend.rs `Gate`) passes no row when the arm's bit is set.
use gravity_lowerv4_it as common;

use gravity_ir::Op;
use gravity_program::protos::ProtoKind;

#[test]
fn pruned_merge_arms_read_their_skip_bits() {
    let text = std::fs::read_to_string(common::plans_dir().join("regress-full/partition_prune-089.plan")).expect("plan fixture");
    let l = common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|e| panic!("partition_prune-089: {e:?}"));
    let prunes: Vec<_> = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Prune).collect();
    assert_eq!(prunes.len(), 1, "one src.prune run over the merge's arms: {prunes:?}");
    let p = prunes[0];
    let (first, count) = ((p.arg >> 32) as u16, ((p.arg >> 48) & 0x7fff) as u16);
    let (_reader, arms) = l.ir.funcs.split_last().expect("functions");
    assert_eq!((first, count as usize), (0, arms.len()), "the run covers every arm from the first ({} arm functions)", arms.len());
    assert_eq!(p.pipeline, 0, "the run starts at the first arm's pipeline");
    let skip_off = p.words.base as i32 * 8;
    for f in arms {
        let reads = f.blocks.iter().flat_map(|b| b.insts.iter()).any(|i| matches!(i.op, Op::Load { offset, .. } if offset == skip_off));
        assert!(reads, "{}: the arm never loads its skip word (offset {skip_off})", f.name);
    }
}
