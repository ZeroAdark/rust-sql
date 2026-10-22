//! Per-open words (lowerv4 plan P8, H17; lowerv3 design §3.6, D210, D229): every face's mechanism declares each word it reads
//! across rows or calls (LIMIT counters, walk and window state, an expanding step's offset), and the frame alone writes them
//! at the open, before any read: a rescan or a re-open starts from the declared values, a resumed call keeps them. When a
//! word is stored, at every open or once per run of a streamed union (D2121), is the lifetime home's (`lifetime::OpenAt`,
//! `lifetime::union_run`).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Ctx,
    State,
    /// The program's frame words (`SiteKind::Words`): kept across calls and a union's arms; `off` is the word's byte offset.
    Words,
}

/// A word's open scope, the lifetime home's: `Open` (every open) or `Union { flag }` (once per run of a streamed union, by
/// the first of its arms to open, which sets frame word `flag`; the flag's own scope resets it). The name stays for the
/// faces that declare their words with it.
pub use crate::lifetime::OpenAt as Scope;

/// One declared word: `what` names it in a refusal; `init` is stored at every open of its scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenWord {
    pub what: &'static str,
    pub base: Base,
    pub off: i32,
    pub ty: Type,
    pub init: i64,
    pub scope: Scope,
}

#[derive(Debug)]
pub struct OpenWords<'m> {
    pub words: PgVec<'m, OpenWord>,
}

impl OpenWord {
    /// A word written at every open of its function.
    pub const fn open(what: &'static str, base: Base, off: i32, ty: Type, init: i64) -> OpenWord {
        OpenWord { what, base, off, ty, init, scope: Scope::Open }
    }

    /// A frame word written once per run of union `flag`, by the first of its arms to open (`lifetime::OpenAt::Union`).
    pub const fn union(what: &'static str, off: i32, ty: Type, init: i64, flag: u16) -> OpenWord {
        OpenWord { what, base: Base::Words, off, ty, init, scope: crate::lifetime::OpenAt::Union { flag } }
    }
}

impl<'m> OpenWords<'m> {
    pub fn new_in(mcx: Mcx<'m>) -> OpenWords<'m> {
        OpenWords { words: PgVec::new_in(mcx) }
    }

    pub fn declare(&mut self, w: OpenWord) {
        self.words.push_in(w);
    }

    /// Every declared word stored at the function's open, before any read; a union's words once per run of the union
    /// (`lifetime::union_run`: while its flag is clear).
    pub fn write_all(&self, b: &mut FunctionBuilder<'_>, node: i32, ctx: Value, state: Option<Value>, words: Option<Value>) {
        let base = |b: Base| match (b, state, words) {
            (Base::Ctx, _, _) => ctx,
            (Base::State, Some(s), _) => s,
            (Base::Words, _, Some(w)) => w,
            (Base::State, None, _) => crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a per-open state word without a state slot" }),
            (Base::Words, _, None) => crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a frame word without the words site" }),
        };
        // D482: one word, two roles (a run reader's mode and a sort end's run) is a lowering bug, never a program
        if self.words.iter().enumerate().any(|(i, a)| self.words[..i].iter().any(|b| (b.base, b.off) == (a.base, a.off) && b.what != a.what)) {
            crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "two roles declare one per-open word (D482)" });
        }
        for w in self.words.iter().filter(|w| w.scope == crate::lifetime::OpenAt::Open) {
            let v = b.iconst(w.ty, w.init);
            b.store(base(w.base), w.off, v);
        }
        let mut flags: PgVec<'_, u16> = self.words.iter().filter_map(|w| w.scope.flag()).collect_in(*self.words.allocator());
        flags.sort_unstable();
        flags.dedup();
        for flag in flags {
            crate::lifetime::union_run(b, base(Base::Words), flag, |b| {
                for w in self.words.iter().filter(|w| w.scope.flag() == Some(flag)) {
                    let v = b.iconst(w.ty, w.init);
                    b.store(base(w.base), w.off, v);
                }
            });
        }
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;

    fn lower(ws: &[OpenWord]) -> Result<(), crate::Refusal> {
        crate::catch(7, || {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("p0", &[Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let ctx = b.params()[0];
            let m = gravity_driver::mem::root::context("test", true);
            let mut w = OpenWords::new_in(m.mcx());
            ws.iter().for_each(|&x| w.declare(x));
            w.write_all(&mut b, 7, ctx, None, Some(ctx));
        })
    }

    /// D482: two roles on one frame word refuse as a Bug at lowering; one role declared twice (a union's arms) does not.
    #[test]
    fn two_roles_on_one_word_refuse() {
        let (run, mode) = (OpenWord::open("sort run", Base::Words, 8, Type::I64, 0), OpenWord::open("sort mode", Base::Words, 8, Type::I64, 0));
        let r = lower(&[run, mode]).expect_err("a shared word lowers");
        assert!(matches!(r.reason, crate::admit::refusal::Reason::Bug { payload } if payload.contains("D482")), "{:?}", r.reason);
        assert!(lower(&[run, OpenWord::open("sort mode", Base::Words, 16, Type::I64, 0)]).is_ok());
        assert!(lower(&[run, run]).is_ok(), "one role declared twice");
        assert!(lower(&[run, OpenWord::open("sort mode", Base::Ctx, 8, Type::I64, 0)]).is_ok(), "another base");
    }
}
