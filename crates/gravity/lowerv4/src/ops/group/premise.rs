//! group's premises (v5 landing 4, moved from props/ops.rs's first-of-run arm): V4R-147: DISTINCT ON's first-of-run
//! step, the road run.pergroup (Applies::DistinctOn); each key's hash arm by its type, unpacked (runs/per_group.rs,
//! D1517: an arm not built refuses by this name). The props walk calls `first_of_run` for each Op::FirstOfRun.
use gravity_decompose::ops::TieDesc;

use crate::applies::Prop;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::FirstOfRun.
    pub(crate) fn first_of_run(&mut self, t: &TieDesc) {
        self.p.add(Prop::DistinctOn);
        for key in t.keys.iter() {
            self.add(key_kind(key.kind, self.image_of(key.typid), false, Accessor::Hash));
        }
    }
}
