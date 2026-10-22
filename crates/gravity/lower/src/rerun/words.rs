use datum::NullableDatum;
use gravity_expr::emit::ParamNeed;
use gravity_program::limits;
use gravity_program::{ArenaRef, ArenaVec};
use types_core::Oid;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

/// Where a value the emitted code reads lives: a ctx parameter word the binder fills, or an
/// immediate the caller argued structural (code-cache.md §1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Word(u16),
    Fold,
    /// A nested pipeline's per-row parameter (`$exec<id>`): the enclosing lowering's cell (`Frame::exec`).
    Exec(i32),
}

/// What the binder puts in a word (the 2e contract): a plan literal, an extern parameter, an
/// expression over them evaluated once at bind (an index key, a LIMIT bound).
#[derive(Clone, Copy, Debug)]
pub enum WordSrc<'p> {
    Const { value: NullableDatum, typ: Oid },
    /// Element `index` of the array literal at the word's node (an `IN` list's member): the server materializes it
    /// from the plan's array node per execution (`admit::WordTpl::Elem`), the rig from `value`.
    Elem { value: NullableDatum, typ: Oid, index: u32 },
    Extern { id: i32, typ: Oid },
    Expr(Node<'p>),
    /// D899: an index key's expression, PostgreSQL's runtime key (evaluated at its scan's first pull: ExecIndexScan ->
    /// ExecReScan -> ExecIndexEvalRuntimeKeys): bound when the pipeline whose site reads it starts (the runner's late
    /// words), so a pipeline PG never starts (a build under an empty outer, a producer under an empty build) never
    /// evaluates it (a user function's call, an error); bound at bind as `Expr` where the runner cannot place it.
    Late(Node<'p>),
    /// A kernel word a `WordsDesc` declares (the runner binds it: a build's spill bits).
    Kernel,
    /// D422: `WHERE CURRENT OF name`'s cursor, bound per execution as the address of this execution's `CurrentOfExpr`
    /// (`admit::WordTpl::Cursor`): the name is never the program's, so one program serves every cursor.
    Cursor(Node<'p>),
    Exec { id: i32, typ: Oid },
    /// V4R-122: the element set of a hashed `= ANY` constant array at `node`, built once per execution at bind for the
    /// program's owned object `desc` (`admit::WordTpl::Set`, lowerv4 step/expr_saop_set.rs `bound`): the word is its address.
    Set { node: Node<'p>, desc: u32 },
    /// D2834: a deferred extern parameter `$id` (gravity_program::externs `deferred`), read where its expression evaluates
    /// it: the word is the execution's parameter list (`admit::WordTpl::Deferred`), which the emitted fetch reads the slot
    /// from (gravity_expr emit/host.rs `param_deferred`), never the slot's value.
    Deferred { id: i32, typ: Oid },
}

pub trait WordPlan<'p> {
    /// A need of an emitted expression, in `Compiled::param_needs` order; `typ` / `byval` from the tree.
    fn need(&mut self, need: &ParamNeed, typ: Oid, byval: bool) -> Place;
    /// `need` with the tree node the need came from (a literal's locator for a plan that files words by node).
    fn need_at(&mut self, need: &ParamNeed, typ: Oid, byval: bool, node: Node<'p>) -> Place {
        let _ = node;
        self.need(need, typ, byval)
    }
    /// `need_at` for element `index` of the array literal `node` (an `IN` list's member, `ParamNeed::Elem`).
    fn need_elem_at(&mut self, need: &ParamNeed, typ: Oid, byval: bool, node: Node<'p>, index: u32) -> Place {
        let _ = index;
        self.need_at(need, typ, byval, node)
    }
    /// An index key's value or a LIMIT bound: a word evaluated once at bind.
    fn key(&mut self, value: Node<'p>) -> Place;
    /// D899: an index key's value bound when its pipeline starts (`WordSrc::Late`); a plan that files no expression word
    /// places it as `key`.
    fn key_late(&mut self, value: Node<'p>) -> Place {
        self.key(value)
    }
    fn nwords(&self) -> usize;
    /// The first `n` words are the kernel words the pipelines' `WordsDesc`s declare (D3).
    fn reserve(&mut self, _n: usize) {}
    /// D3642: literal `node` reads `word`, placed for another copy of it (lowerv4 `Px::lit_word`).
    fn follow(&mut self, word: Place, node: Node<'p>) {
        let _ = (word, node);
    }
    /// An InitPlan's result word, declared before any expression over it compiles.
    fn init_param(&mut self, id: i32, typ: Oid) -> Place {
        let _ = (id, typ);
        Place::Fold
    }
    /// A per-row parameter of a nested pipeline: its needs place as `Place::Exec(id)` from now on.
    fn nest(&mut self, id: i32) {
        let _ = id;
    }
    /// D422: the cursor of `WHERE CURRENT OF` (a `CurrentOfExpr`) as a word bound per execution: a refcursor parameter's
    /// value (`cursor_param`), else the execution's node (`WordSrc::Cursor`, its `cursor_name`). A plan that files no
    /// words by node folds (the cursor is then no word: the caller refuses).
    fn cursor(&mut self, node: Node<'p>) -> Place {
        let _ = node;
        Place::Fold
    }
    /// V4R-122: a hashed `= ANY` constant's element set, built once per execution at bind from the array at `node` for
    /// the program's owned object `desc` (`WordSrc::Set`). A plan that files no words by node folds (the caller refuses).
    fn set(&mut self, node: Node<'p>, desc: u32) -> Place {
        let _ = (node, desc);
        Place::Fold
    }
}

/// `refcursor`: a `CURRENT OF` parameter's type.
const REFCURSOROID: Oid = 1790;

/// The default plan: every literal occurrence its own word (two equal literals are two words — a cached program's later execution may carry different literals in the two places; a
/// by-reference literal's word is its pointer, per statement — folded, the pointer was an immediate
/// and the IR's content, the code cache's key, differed per backend), every extern parameter one word.
pub struct Sequential<'p> {
    pub words: ArenaVec<WordSrc<'p>>,
    pub overflow: bool,
    pub nested: ArenaVec<i32>,
    /// D2834: a reader took a deferred extern parameter's value ahead of its evaluation (a key, a count, a table bound at
    /// bind or at a pipeline's start): the admission refuses the statement by name (`DEFERRED_AHEAD`).
    pub ahead: bool,
}

/// D2834: the refusal of a statement whose deferred extern parameter a reader takes ahead of its evaluation
/// (`Sequential::ahead`).
pub const DEFERRED_AHEAD: &str = "D2834: a PL parameter left to evaluation time read ahead of its evaluation (a key, a count or a table bound before a row reaches it)";

/// D2834: whether expression `n` reads a deferred extern parameter (gravity_program::externs `deferred`).
pub fn reads_deferred(n: Node<'_>) -> bool {
    let mut found = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        found |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN && gravity_program::externs::deferred(p.paramid));
        !found
    });
    found
}

impl<'p> Sequential<'p> {
    pub fn new(arena: ArenaRef) -> Sequential<'p> {
        Sequential { words: ArenaVec::new_in(arena), overflow: false, nested: ArenaVec::new_in(arena), ahead: false }
    }

    fn push(&mut self, w: WordSrc<'p>) -> Place {
        if self.words.len() >= limits::PARAM_WORDS {
            self.overflow = true;
            return Place::Fold;
        }
        self.words.push(w);
        Place::Word(self.words.len() as u16 - 1)
    }

    /// `key` / `key_late`: a constant or a parameter is its own word as ever; an expression is an `Expr` word, or with
    /// `late` a `Late` one (D899; a by-reference constant stays `Expr`: PostgreSQL's scan key, never a runtime key).
    fn key_word(&mut self, value: Node<'p>, late: bool) -> Place {
        if let Some(c) = value.as_const() {
            if c.constbyval || c.constisnull {
                let nd = if c.constisnull { NullableDatum::null() } else { NullableDatum::value(c.constvalue) };
                return self.need(&ParamNeed::Const(nd), c.consttype, true);
            }
        }
        if let Some(p) = value.as_param() {
            if p.paramkind == ParamKind::PARAM_EXTERN {
                return self.need(&ParamNeed::Extern { id: p.paramid, typ: p.paramtype }, p.paramtype, true);
            }
            if p.paramkind == ParamKind::PARAM_EXEC {
                return self.need(&ParamNeed::Exec { id: p.paramid, typ: p.paramtype }, p.paramtype, true);
            }
        }
        if has_exec(value) {
            return Place::Fold;
        }
        // D2834: an expression word is evaluated at bind (or its pipeline's start), ahead of the rows
        if reads_deferred(value) {
            self.ahead = true;
        }
        // D2470: one expression node is one word: two readers of one node (a Param LIMIT's Limit step and the feeder's
        // check of it) share its value, as the key traverses the node once per place it holds in the graph
        let same = |w: &WordSrc<'p>| match *w {
            WordSrc::Expr(n) => !late && n.ptr_eq(value),
            WordSrc::Late(n) => late && n.ptr_eq(value),
            _ => false,
        };
        if let Some(i) = self.words.iter().position(same) {
            return Place::Word(i as u16);
        }
        self.push(if late && value.as_const().is_none() { WordSrc::Late(value) } else { WordSrc::Expr(value) })
    }

    /// Element `index` of an array literal as its own word (`WordSrc::Elem`), like a literal: a by-reference
    /// element's word is its pointer, bound per execution (folded, the IR's content differed per backend).
    pub fn need_elem(&mut self, need: &ParamNeed, typ: Oid, index: u32) -> Place {
        let ParamNeed::Elem(value) = *need else { return self.need(need, typ, false) };
        self.push(WordSrc::Elem { value, typ, index })
    }
}

impl<'p> WordPlan<'p> for Sequential<'p> {
    fn need_elem_at(&mut self, need: &ParamNeed, typ: Oid, byval: bool, node: Node<'p>, index: u32) -> Place {
        let _ = (node, byval);
        self.need_elem(need, typ, index)
    }

    fn need(&mut self, need: &ParamNeed, typ: Oid, byval: bool) -> Place {
        match *need {
            ParamNeed::Const(value) | ParamNeed::Elem(value) => {
                let _ = byval;
                self.push(WordSrc::Const { value, typ })
            }
            ParamNeed::Extern { id, typ } => {
                // D2834: the slot's value as a word, ahead of the expression that would evaluate it
                if gravity_program::externs::deferred(id) {
                    self.ahead = true;
                }
                let same = self.words.iter().position(|w| matches!(w, WordSrc::Extern { id: i, .. } if *i == id));
                match same {
                    Some(i) => Place::Word(i as u16),
                    None => self.push(WordSrc::Extern { id, typ }),
                }
            }
            // D2834: one word, the execution's list, for every read of a deferred parameter
            ParamNeed::Deferred { id, typ } => match self.words.iter().position(|w| matches!(w, WordSrc::Deferred { .. })) {
                Some(i) => Place::Word(i as u16),
                None => self.push(WordSrc::Deferred { id, typ }),
            },
            ParamNeed::Exec { id, .. } => match self.words.iter().position(|w| matches!(w, WordSrc::Exec { id: i, .. } if *i == id)) {
                Some(i) => Place::Word(i as u16),
                None if self.nested.contains(&id) => Place::Exec(id),
                None => Place::Fold,
            },
        }
    }

    fn init_param(&mut self, id: i32, typ: Oid) -> Place {
        match self.words.iter().position(|w| matches!(w, WordSrc::Exec { id: i, .. } if *i == id)) {
            Some(i) => Place::Word(i as u16),
            None => self.push(WordSrc::Exec { id, typ }),
        }
    }

    fn nest(&mut self, id: i32) {
        if !self.nested.contains(&id) {
            self.nested.push(id);
        }
    }

    fn key(&mut self, value: Node<'p>) -> Place {
        self.key_word(value, false)
    }

    fn key_late(&mut self, value: Node<'p>) -> Place {
        self.key_word(value, true)
    }

    fn nwords(&self) -> usize {
        self.words.len()
    }

    fn cursor(&mut self, node: Node<'p>) -> Place {
        match node.as_current_of_expr() {
            Some(c) if c.cursor_name.is_none() => self.need(&ParamNeed::Extern { id: c.cursor_param, typ: REFCURSOROID }, REFCURSOROID, false),
            Some(_) => self.push(WordSrc::Cursor(node)),
            None => Place::Fold,
        }
    }

    fn reserve(&mut self, n: usize) {
        // D1937: past the bound `push` files nothing (the plan overflows, the statement refuses): stop there, never spin
        while self.words.len() < n && self.push(WordSrc::Kernel) != Place::Fold {}
    }

    fn set(&mut self, node: Node<'p>, desc: u32) -> Place {
        self.push(WordSrc::Set { node, desc })
    }
}

/// Whether an expression reads an executor parameter (a bind-time word cannot hold it).
pub fn has_exec(n: Node<'_>) -> bool {
    let mut found = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        found |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC);
        !found
    });
    found
}

/// A fixed placement (the probe's install path): a literal sits at the admission's slot, an extern
/// parameter at its slot, the `k`-th index key at `keys[k]`; a literal the admission did not fix folds.
pub struct Fixed<'a> {
    pub consts: &'a [(u64, u8)],
    pub externs: &'a [(i32, u8)],
    pub keys: &'a [i16],
    pub next_key: usize,
}

impl<'p> WordPlan<'p> for Fixed<'_> {
    fn need(&mut self, need: &ParamNeed, _typ: Oid, byval: bool) -> Place {
        match *need {
            ParamNeed::Const(v) | ParamNeed::Elem(v) if byval && !v.isnull => self.consts.iter().find(|(c, _)| *c == v.value.as_u64()).map_or(Place::Fold, |&(_, s)| Place::Word(s as u16)),
            ParamNeed::Extern { id, .. } => self.externs.iter().find(|(i, _)| *i == id).map_or(Place::Fold, |&(_, s)| Place::Word(s as u16)),
            _ => Place::Fold,
        }
    }

    fn key(&mut self, _value: Node<'p>) -> Place {
        let k = self.next_key;
        self.next_key += 1;
        match self.keys.get(k) {
            Some(&s) if s >= 0 => Place::Word(s as u16),
            _ => Place::Fold,
        }
    }

    fn nwords(&self) -> usize {
        self.consts.len() + self.externs.len() + self.keys.len()
    }
}

/// The `Sequential` plan with every word's node beside it (`need_at` / `key`); a literal need
/// without a node (a LIMIT constant, a verdict substitution) folds — the key carries its value.
pub struct Located<'p> {
    pub seq: Sequential<'p>,
    pub nodes: ArenaVec<Option<Node<'p>>>,
    /// D3642: the literals that read a word placed for another copy (`follow`): exec admit's template keeps each inside
    /// the key's traversal, where the keys hold it as the word's node.
    pub followers: ArenaVec<Node<'p>>,
}

impl<'p> Located<'p> {
    pub fn new(arena: ArenaRef) -> Located<'p> {
        Located { seq: Sequential::new(arena), nodes: ArenaVec::new_in(arena), followers: ArenaVec::new_in(arena) }
    }

    fn grown(&mut self, at: Option<Node<'p>>) {
        while self.nodes.len() < self.seq.nwords() {
            self.nodes.push(at);
        }
    }
}

impl<'p> WordPlan<'p> for Located<'p> {
    fn need(&mut self, need: &ParamNeed, typ: Oid, byval: bool) -> Place {
        if matches!(need, ParamNeed::Const(_) | ParamNeed::Elem(_)) {
            return Place::Fold;
        }
        let p = self.seq.need(need, typ, byval);
        self.grown(None);
        p
    }

    fn need_at(&mut self, need: &ParamNeed, typ: Oid, byval: bool, node: Node<'p>) -> Place {
        let p = self.seq.need(need, typ, byval);
        self.grown(Some(node));
        p
    }

    fn need_elem_at(&mut self, need: &ParamNeed, typ: Oid, byval: bool, node: Node<'p>, index: u32) -> Place {
        let _ = byval;
        let p = self.seq.need_elem(need, typ, index);
        self.grown(Some(node));
        p
    }

    fn key(&mut self, value: Node<'p>) -> Place {
        if let Some(c) = value.as_const() {
            if c.constbyval || c.constisnull {
                let nd = if c.constisnull { NullableDatum::null() } else { NullableDatum::value(gravity_expr::nodes::konst::word_canon(c.consttype, c.constvalue)) };
                return self.need_at(&ParamNeed::Const(nd), c.consttype, true, value);
            }
        }
        let p = self.seq.key(value);
        self.grown(Some(value));
        p
    }

    fn key_late(&mut self, value: Node<'p>) -> Place {
        if value.as_const().is_some() {
            return self.key(value);
        }
        let p = self.seq.key_late(value);
        self.grown(Some(value));
        p
    }

    fn nwords(&self) -> usize {
        self.seq.nwords()
    }

    fn reserve(&mut self, n: usize) {
        self.seq.reserve(n);
        self.grown(None);
    }

    fn follow(&mut self, word: Place, node: Node<'p>) {
        if matches!(word, Place::Word(_)) {
            self.followers.push(node); // alloc: Program
        }
    }

    fn init_param(&mut self, id: i32, typ: Oid) -> Place {
        let p = self.seq.init_param(id, typ);
        self.grown(None);
        p
    }

    fn nest(&mut self, id: i32) {
        self.seq.nest(id);
    }

    fn cursor(&mut self, node: Node<'p>) -> Place {
        let p = self.seq.cursor(node);
        self.grown(Some(node));
        p
    }

    fn set(&mut self, node: Node<'p>, desc: u32) -> Place {
        let p = self.seq.set(node, desc);
        self.grown(Some(node));
        p
    }
}
