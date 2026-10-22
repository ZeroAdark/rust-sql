//! The pulled set's runtime (M8 RT-2f; the exact request, PLAN-v5 §2.5): a deferred set of pipelines (the pulled one last,
//! with its exclusive producers) that a host runs on through the runner's frame (`OnDemand::pull_members`) until the
//! pulled pipeline's end stops resumably after the rows its pull wants, or ends. Its users: merge.join's pulled inner
//! (jmerge-0012, exec ops/mergejoin/proto.rs) and window W4's pulled fill, whose `Pull` (exec ops/window/proto.rs) this
//! is, hoisted so the two share one runtime (main 2026-10-03T20:50Z); the window lane moves its fill onto it. The frame
//! call itself is `unsafe` and stays in each user's family raw.rs (R6), handed in as a `RawPull`.
use core::cell::Cell;

use gravity_driver::{Frame, Pulled};
use gravity_program::Program;
use types_error::PgResult;

use crate::glue;

/// A family raw.rs's frame call: `members` run on through `frame` (`OnDemand::pull_members(members, 1)`, RT-2f).
pub type RawPull = fn(Frame, &[u16]) -> gravity_driver::Result<Pulled>;

/// One pulled set: its members in program order (the pulled pipeline last), run only through the runner's frame; `what`
/// names its owner in its errors.
pub struct Pull {
    members: Box<[u16]>, // alloc: Exec
    frame: Cell<Option<Frame>>,
    running: Cell<bool>,
    done: Cell<bool>,
    failed: Cell<bool>,
    what: &'static str,
    raw: RawPull,
}

impl Pull {
    /// Pipeline `k`'s pulled set (`Program::members`: `k` and its exclusive producers), in program order, `k` last, pulled
    /// through `raw`.
    pub fn new(program: &Program, k: usize, what: &'static str, raw: RawPull) -> PgResult<Pull> {
        let mut m: Vec<u16> = program.members(k).members; // alloc: Exec
        m.sort_unstable();
        m.dedup();
        if m.last() != u16::try_from(k).ok().as_ref() {
            return Err(glue::internal(&format!("gravity: {what}: its pulled set does not end in its pipeline"))); // alloc: Cold
        }
        Ok(Pull { members: m.into_boxed_slice(), frame: Cell::new(None), running: Cell::new(false), done: Cell::new(false), failed: Cell::new(false), what, raw })
    }

    /// The runner steps past `pipeline` in the producer order (`Live::deferred`): a member.
    pub fn deferred(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|x| self.members.contains(&x))
    }

    /// The runner's frame while it runs the program (`Live::frame`).
    pub fn frame(&self, f: Option<Frame>) {
        self.frame.set(f);
    }

    /// One pull: true when the pulled pipeline stopped after the rows its pull wants, false once it ended (sticky); an
    /// error once is every later pull's, by name.
    pub fn pull(&self) -> PgResult<bool> {
        if self.done.get() {
            return Ok(false);
        }
        if self.failed.get() {
            return Err(glue::internal(&format!("gravity: {} read again after its error (M8 RT-2f)", self.what))); // alloc: Cold
        }
        if self.running.get() {
            return Err(glue::internal(&format!("gravity: {} pulled inside its own run", self.what))); // alloc: Cold
        }
        let Some(f) = self.frame.get() else {
            return Err(glue::internal(&format!("gravity: {}: a pull with no runner frame (the lowering pins its reader to the leader)", self.what))); // alloc: Cold
        };
        self.running.set(true);
        let r = (self.raw)(f, &self.members);
        self.running.set(false);
        match r {
            Ok(gravity_driver::Pulled::Done) => {
                self.done.set(true);
                Ok(false)
            }
            Ok(gravity_driver::Pulled::Stopped) => Ok(true),
            Err(e) => {
                self.failed.set(true);
                Err(glue::pg_error(e))
            }
        }
    }
}
