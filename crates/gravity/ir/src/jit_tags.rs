//! The JIT map's tags (gravity_backend `perfmap`, mode `tags`): what a lowering says each range of its IR does (a plan
//! node's mechanism, the body form it runs in), kept out of the `Program`: the code key is the IR's content, a tag is the
//! lowering's. Recorded only when the lowering asks (`ProgramBuilder::jit_tags`); free at run time (the backend turns
//! them into perf map names at the compile, the code unchanged).

use crate::{raw, IrVec};
use core::fmt;
use mcx::SendRoot;

/// From instruction `idx` of `block` in function `func` on (the terminator is index `insts.len()`), the block's
/// instructions carry `tag`, until the block's next run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitRun {
    pub func: u32,
    pub block: u32,
    pub idx: u32,
    pub tag: u32,
}

/// A program's tags: each tag's name (tag 0 is `frame`, the function's own code) and the runs, each place at most once.
/// Every vector lives in the tags' own root context (`root`), named at its birth for the lowering's owner and adopted by
/// the perf map's notes when they file it (`set_owner`), so its bytes are an owner's by construction. The fields drop in
/// order, every vector before the root.
pub struct JitTags {
    /// The names back to back, and where each ends.
    text: IrVec<u8>,
    ends: IrVec<u32>,
    runs: IrVec<JitRun>,
    /// While recording (`JitState`): each block of the function being built, its tag in effect and its last run's index
    /// (`u32::MAX`: none yet). Emptied when the recording ends.
    last: IrVec<(u32, u32)>,
    root: SendRoot,
}

const ROOT_NAME: &str = "GravityJitTags";

impl JitTags {
    /// No runs and one tag, `frame` (0), in a new root named `owner`'s at its birth (`None`: the creating thread's).
    pub fn new(owner: Option<memledger::Owner>) -> JitTags {
        let mut root = match owner {
            Some(o) => SendRoot::new_owned_small(ROOT_NAME, o),
            None => SendRoot::new_small(ROOT_NAME),
        };
        let (text, ends) = (IrVec::new_in(raw::tags_handle(&mut root)), IrVec::new_in(raw::tags_handle(&mut root)));
        let (runs, last) = (IrVec::new_in(raw::tags_handle(&mut root)), IrVec::new_in(raw::tags_handle(&mut root)));
        let mut t = JitTags { text, ends, runs, last, root };
        t.intern(format_args!("frame"));
        t
    }

    /// Its bytes on `owner` from now (the perf map's notes adopting it).
    pub fn set_owner(&mut self, owner: memledger::Owner) {
        self.root.set_owner(owner);
    }

    /// Bytes of its root's blocks.
    pub fn bytes(&self) -> usize {
        self.root.allocated()
    }

    /// How many tags it names.
    pub fn names(&self) -> u32 {
        self.ends.len() as u32
    }

    pub fn name(&self, tag: u32) -> &str {
        let i = tag as usize;
        let Some(&end) = self.ends.get(i) else { return "untagged" };
        let lo = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        core::str::from_utf8(&self.text[lo..end as usize]).unwrap_or("untagged")
    }

    /// The tag `name` writes, interned.
    pub fn intern(&mut self, name: fmt::Arguments<'_>) -> u32 {
        let at = self.text.len();
        let _ = fmt::Write::write_fmt(&mut Text(&mut self.text), name);
        let mut lo = 0;
        let mut found = None;
        for (i, &e) in self.ends.iter().enumerate() {
            if self.text[lo..e as usize] == self.text[at..] {
                found = Some(i as u32);
                break;
            }
            lo = e as usize;
        }
        match found {
            Some(i) => {
                self.text.truncate(at);
                i
            }
            None => {
                self.ends.push(self.text.len() as u32);
                self.ends.len() as u32 - 1
            }
        }
    }

    /// Adds `run` (a place at most once: a hand-made table's; the builder records through `JitState`).
    pub fn push(&mut self, run: JitRun) {
        self.runs.push(run);
    }

    /// Every run, in recording order (a function's by block then index: sort them).
    pub fn runs(&self) -> &[JitRun] {
        &self.runs
    }
}

impl fmt::Debug for JitTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JitTags").field("names", &self.ends.len()).field("runs", &self.runs.len()).finish()
    }
}

/// A name's bytes, appended to the tags' text.
struct Text<'a>(&'a mut IrVec<u8>);

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// The builder's recording state.
pub(crate) struct JitState {
    pub(crate) tags: JitTags,
    /// The tag in effect.
    pub(crate) cur: u32,
    /// The function being built.
    func: u32,
}

impl JitState {
    pub(crate) fn new(owner: Option<memledger::Owner>) -> JitState {
        JitState { tags: JitTags::new(owner), cur: 0, func: 0 }
    }

    pub(crate) fn define(&mut self, func: u32) {
        (self.func, self.cur) = (func, 0);
        self.tags.last.clear();
    }

    /// Instruction `idx` of `block` is about to be added: a run starts there when the block's last run has another tag
    /// (at the same place, that run takes the tag: no instruction carried the old one).
    #[inline]
    pub(crate) fn at(&mut self, block: u32, idx: u32) {
        let (b, t) = (block as usize, &mut self.tags);
        if t.last.len() <= b {
            t.last.resize(b + 1, (u32::MAX, u32::MAX));
        }
        let (tag, run) = t.last[b];
        if tag == self.cur {
            return;
        }
        if t.runs.get(run as usize).is_some_and(|r| r.idx == idx) {
            t.runs[run as usize].tag = self.cur;
        } else {
            t.last[b].1 = t.runs.len() as u32;
            t.runs.push(JitRun { func: self.func, block, idx, tag: self.cur });
        }
        t.last[b].0 = self.cur;
    }

    /// `n` instructions were inserted at `idx` of `block`: the runs after it move down.
    pub(crate) fn inserted(&mut self, block: u32, idx: u32, n: u32) {
        let func = self.func;
        self.tags.runs.iter_mut().filter(|r| r.func == func && r.block == block && r.idx > idx).for_each(|r| r.idx += n);
    }

    /// The tag `name` writes (interned) in effect from here on; the tag before it returned.
    pub(crate) fn set(&mut self, name: fmt::Arguments<'_>) -> u32 {
        let id = self.tags.intern(name);
        core::mem::replace(&mut self.cur, id)
    }

    /// The tags recorded, the recording's state dropped.
    pub(crate) fn finish(mut self) -> JitTags {
        self.tags.last.clear();
        self.tags.last.shrink_to_fit();
        self.tags
    }
}

#[cfg(test)]
mod tests {
    use super::JitRun;
    use crate::builder::ProgramBuilder;
    use crate::*;
    use alloc::vec::Vec;

    fn build(on: bool) -> (Program, Option<super::JitTags>) {
        let mut pb = ProgramBuilder::new();
        if on {
            pb.jit_tags();
        }
        let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
        {
            let mut b = pb.define(f);
            let x = b.params()[0];
            let l = b.create_block(&[Type::I64]);
            let t = b.set_tag(format_args!("a"));
            assert_eq!(t, 0);
            let c = b.iconst(Type::I64, 7);
            let y = b.int_bin(IntBinOp::Add, x, c);
            b.br(l, &[y]);
            b.switch_to(l);
            let u = b.set_tag(format_args!("{}", "b"));
            assert_eq!(u, if on { 1 } else { 0 });
            let p = b.block_params(l)[0];
            let d = b.iconst(Type::I64, 1);
            b.restore_tag(u);
            let z = b.int_bin(IntBinOp::Add, p, d);
            b.restore_tag(t);
            b.ret(&[z]);
        }
        let tags = pb.take_jit_tags();
        (pb.finish(), tags)
    }

    fn of(t: &super::JitTags, func: u32) -> Vec<JitRun> {
        let mut v: Vec<JitRun> = t.runs().iter().copied().filter(|r| r.func == func).collect();
        v.sort_unstable_by_key(|r| (r.block, r.idx));
        v
    }

    /// Each block's runs follow the tag in effect when its instructions and terminator were added; the IR is the same
    /// with tags off, and off records nothing.
    #[test]
    fn runs_follow_the_tag_in_effect() {
        let (on, tags) = build(true);
        let (off, none) = build(false);
        assert_eq!(on, off, "tags change no IR");
        assert!(none.is_none());
        let t = tags.expect("tags");
        assert_eq!((0..t.names()).map(|i| t.name(i)).collect::<Vec<_>>(), ["frame", "a", "b"]);
        let r = |block, idx, tag| JitRun { func: 0, block, idx, tag };
        assert_eq!(of(&t, 0), [r(0, 0, 1), r(1, 0, 2), r(1, 1, 1), r(1, 2, 0)]);
        assert_eq!((t.name(2), t.name(9)), ("b", "untagged"));
        assert!(t.bytes() > 0);
    }

    #[test]
    fn an_insert_moves_the_runs_after_it() {
        let mut j = super::JitState::new(None);
        j.define(0);
        j.at(0, 0);
        j.cur = 1;
        j.at(0, 3);
        j.inserted(0, 2, 2);
        assert_eq!(j.tags.runs(), [JitRun { func: 0, block: 0, idx: 0, tag: 0 }, JitRun { func: 0, block: 0, idx: 5, tag: 1 }]);
    }

    /// A second tag at the same place retags the run there; interning returns the same tag for the same name.
    #[test]
    fn a_place_holds_one_run() {
        let mut j = super::JitState::new(None);
        j.define(0);
        j.at(0, 0);
        let a = j.set(format_args!("x.{}", 1));
        assert_eq!(a, 0);
        j.at(0, 1);
        j.set(format_args!("y"));
        j.at(0, 1);
        assert_eq!(j.set(format_args!("x.1")), 2);
        assert_eq!(j.tags.runs(), [JitRun { func: 0, block: 0, idx: 0, tag: 0 }, JitRun { func: 0, block: 0, idx: 1, tag: 2 }]);
        let t = j.finish();
        assert_eq!((t.names(), t.name(1), t.name(2)), (3, "x.1", "y"));
    }
}
