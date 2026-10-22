//! scan's premises (v5 landing 4, moved from props/ops.rs's source walk): a pipeline source's roads (Seq, Heap, Pgrc,
//! Code, Packed) and kinds (index arrays, bitmap trees, a lossy recheck's expressions, each lane's datum arm). The props
//! walk calls `source` once per pipeline.
use gravity_decompose::catalog::IndexAm;
use gravity_decompose::graph::KeyForm;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::{PipelineNode, Source};

use crate::applies::Prop;
use crate::face::kinds::KindId;
use crate::props::Walk;
use crate::rep::datum_kind;

impl<'p> Walk<'_, '_, 'p> {
    pub(crate) fn source(&mut self, p: &PipelineNode<'p>) {
        match &p.source {
            Source::Range { .. } => self.p.add(Prop::Seq),
            Source::Index { keys, recheck, iam, .. } => {
                if keys.iter().any(|key| matches!(key.form, KeyForm::Any | KeyForm::AmAny)) {
                    self.add(KindId::SrcIndexArray);
                }
                // a btree walk's keys are exact: PostgreSQL evaluates indexqualorig only under EvalPlanQual (IndexRecheck),
                // which the host's key test answers (hosts IndexSource::next_substituted); no program compiles it. An
                // index-only scan's recheckqual likewise. Another AM's lossy recheck is compiled (src.index_am).
                if *iam != IndexAm::Btree {
                    self.list(recheck);
                }
            }
            Source::Bitmap { plan, recheck, .. } => {
                if matches!(plan, BitmapPlan::And { .. } | BitmapPlan::Or { .. }) {
                    self.add(KindId::SrcBitmapTree);
                }
                self.list(recheck);
            }
            // D2907: a list that streams past the coded cells' cost: PostgreSQL's evaluator takes its cells, no code
            // D4449: a row of Consts names no premise (a Const is no call, CASE, `= ANY`, whole-row reference or json
            // call), so only the rows holding another cell are walked: a literal table's rows cost no walk each, and a
            // list of Consts alone (`consts_only`) no walk at all
            Source::Values { lists, .. } if !crate::ops::shared::values::consts_only(self.gx.graph, lists) && !crate::ops::shared::values::streams_past_cut(lists) => {
                for l in lists.iter().filter(|l| !crate::ops::shared::values::all_consts(*l)) {
                    self.expr(l);
                }
            }
            // a TID clause's value side is a key word the bind evaluates once, as an index key's (source/tid.rs): the
            // quals are not compiled expressions, so `ctid = ANY (...)` names no expr.saop (D392)
            Source::Tid { .. } => {}
            _ => {}
        }
        if let Some((_, _, site)) = p.source.relation() {
            self.p.add(if matches!(p.source, Source::Range { am: gravity_decompose::Am::Pgrcolumnar, .. }) { Prop::Pgrc } else { Prop::Heap });
            for d in site.lanes.iter() {
                let arm = datum_kind(d.rep);
                self.add(arm);
                match arm {
                    KindId::RepCodeDatum => self.p.add(Prop::Code),
                    KindId::RepPackedDatum => self.p.add(Prop::Packed),
                    _ => {}
                }
            }
        }
    }
}
