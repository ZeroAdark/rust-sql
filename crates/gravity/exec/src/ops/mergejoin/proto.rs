//! merge.join's runtime protocol (jmerge-0012, the exact request's second consumer): `Markable` with `MERGE_PULLED`, a
//! merge join's pulled inner. PostgreSQL's ExecMergeJoin fetches its inner one tuple at a time and stops where the join
//! ends; lowerv4's merge (ops/mergejoin/exact_inner.rs) reads its inner's spool while it fills and, past the stored rows,
//! calls `m11_pull(h)`: the inner's set (its spool pipeline, last, and its exclusive producers) runs on through the
//! runner's frame (the shared pulled-set runtime, exec pulled/) until the spool's end stops resumably after one more row,
//! or ends. The set is deferred (the producer order steps past it), so an inner the merge never reaches never runs, as
//! PG's. The object lives in the protocol's `Live` (its handle laid in the proto's word) and drops at the execution's end.
//! MJ-AHEAD (the budgeted pull of a calm inner, lowerv4 exact_inner.rs `Form::Grows`): a pull stores up to the merge's
//! want (a budget doubling from one row), and an error it raised after the row the merge wanted is held (`m11_hold`,
//! `holdable`) for the next pull, as PG raises it only when its merge fetches the row that raised.
use std::rc::Rc;

use gravity_program::protos::{Proto, ProtoKind, MERGE_PULLED};
use types_error::{PgError, PgResult};

use crate::glue;
use crate::ops::{Frame, Live, Prepare, Protocol};
use crate::pulled::Pull;

pub const PROTOCOLS: &[&dyn Protocol] = &[&Markable];

pub struct Markable;

struct MergeLive {
    _cell: Box<super::raw::PullCell>,
    pull: Rc<Pull>,
    k: usize,
    readers: memledger::ChargedVec<usize>,
}

impl Live for MergeLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::PULL_READERS
    }

    fn deferred(&self, pipeline: usize) -> bool {
        self.pull.deferred(pipeline)
    }

    fn frame(&self, f: Option<Frame>) {
        self.pull.frame(f);
    }

    fn pull_readers(&self, pipeline: usize) -> Option<&[usize]> {
        (pipeline == self.k && !self.readers.is_empty()).then(|| &self.readers[..])
    }
}

impl Protocol for Markable {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Markable
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        if p.arg & MERGE_PULLED == 0 {
            return Err(glue::internal("gravity: merge.markable declared without its pulled inner (no other form has landed)"));
        }
        let (base, k) = (p.words.base as usize, p.pipeline as usize);
        if p.words.n != 1 || base >= cx.words.len() || base >= cx.nulls.len() || k >= cx.program.pipelines.len() {
            return Err(glue::internal("gravity: merge.join's pulled inner is not one handle word over one of the program's pipelines"));
        }
        let pull = Rc::new(Pull::new(cx.program, k, "merge.join's pulled inner", super::raw::pull)?); // alloc: Exec
        let p2 = pull.clone();
        let cell = super::raw::PullCell::new(Box::new(move || p2.pull())); // alloc: Exec
        cx.words[base] = cell.handle();
        cx.nulls[base] = 0;
        *cx.nparams = (*cx.nparams).max(base + 1);
        let mut readers = cx.charged_vec();
        readers.with_vec(|v| v.extend(cx.program.consumers_of(gravity_program::LifeId(p.pipeline)))); // alloc: Exec charged cx.owner
        Ok(Some(Box::new(MergeLive { _cell: cell, pull, k, readers }))) // alloc: Exec
    }
}

/// MJ-AHEAD (as window W10's store and setop's generation `holdable`): an error PostgreSQL raises at the inner row whose
/// code raised it, which a budgeted pull reads ahead of the merge: not a cancel, a resource, system or internal error, nor
/// a refusal, which the statement raises wherever it meets them.
pub(super) fn holdable(e: &PgError) -> bool {
    use types_error::{errcode_to_category as class, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR, ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR};
    let c = class(e.sqlstate());
    e.level() == types_error::ERROR
        && ![ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR].iter().any(|&k| class(k) == c)
        && crate::runner::refusal_of(e).is_none()
}
