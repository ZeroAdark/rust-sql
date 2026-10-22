// build_column_default's stringToNode(adbin), side-cached per backend (the rules / deform_jit pattern): each default is
// read once into the relation's own "relation defaults" context, a CacheMemoryContext child (the relcache's owner), and
// handed out as a copy, as C's per-use read hands every caller a private tree. A tree serves only the exact adbin text
// it was read from; any invalidation of the relation drops the set with its context.
use std::cell::RefCell;
use std::rc::Rc;

use mcx::{Mcx, PgVec, PinnedContext};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::Node;

use crate::with_state;

pub struct RdDefaults {
    // Made on the first fill through `mcx()`. An error after the set is cached (a tree that fails to read) leaves its
    // partial allocations in `cxt` until the next invalidation of the relation drops the set.
    trees: RefCell<Option<PgVec<'static, DefTree>>>,
    // Declared last: the vector goes before the context it lives in (PinnedContext::handle's contract).
    cxt: PinnedContext,
}

struct DefTree {
    adnum: i16,
    adbin: &'static [u8],
    tree: Node<'static>,
}

impl RdDefaults {
    fn new(relid: Oid) -> Rc<RdDefaults> {
        let cxt = PinnedContext::new(with_state(|st| st.root).new_child_small("relation defaults"));
        if let Some((rel, _)) = crate::store::lookup_ent(relid) {
            cxt.set_ident(Some(rel.name()));
        }
        Rc::new(RdDefaults { trees: RefCell::new(None), cxt })
    }

    fn mcx(&self) -> Mcx<'static> {
        // SAFETY: everything allocated through this handle is reachable only through this RdDefaults (callers get
        // copy_object copies), and `trees` drops before `cxt`.
        unsafe { self.cxt.handle() }
    }
}

enum Lookup {
    Hit(Node<'static>),
    Stale,
    Absent,
}

impl RdDefaults {
    fn lookup(&self, adnum: i16, adbin: &str) -> Lookup {
        let trees = self.trees.borrow();
        let found = trees
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|t| t.adnum == adnum)
            .map(|t| (t.adbin == adbin.as_bytes(), t.tree));
        match found {
            Some((true, tree)) => Lookup::Hit(tree),
            Some((false, _)) => Lookup::Stale,
            None => Lookup::Absent,
        }
    }
}

/// The parsed default of column `adnum` of `relid` whose pg_attrdef.adbin is `adbin`, copied into `mcx`.
pub fn RelationGetDefaultExpr<'mcx>(mcx: Mcx<'mcx>, relid: Oid, adnum: i16, adbin: &str) -> PgResult<Node<'mcx>> {
    let cached = with_state(|st| st.defaults_cache.get(&relid).cloned());
    let set = match cached {
        Some(s) => match s.lookup(adnum, adbin) {
            Lookup::Hit(tree) => return copyfuncs::copy_object(mcx, tree),
            Lookup::Absent => s,
            Lookup::Stale => RdDefaults::new(relid),
        },
        None => RdDefaults::new(relid),
    };
    with_state(|st| st.defaults_cache.insert(relid, Rc::clone(&set)));
    let dmcx = set.mcx();
    let tree = readfuncs::stringToNode(dmcx, adbin)?;
    let text: &'static [u8] = mcx::slice_in(dmcx, adbin.as_bytes())?.leak();
    let mut trees = set.trees.borrow_mut();
    match trees.as_mut() {
        Some(v) => v.push(DefTree { adnum, adbin: text, tree }),
        None => {
            let mut v = mcx::vec_with_capacity_in(dmcx, 2)?;
            v.push(DefTree { adnum, adbin: text, tree });
            *trees = Some(v);
        }
    }
    drop(trees);
    copyfuncs::copy_object(mcx, tree)
}

pub(crate) fn forget(relid: Oid) {
    with_state(|st| st.defaults_cache.remove(&relid));
}

#[cfg(test)]
mod tests {
    use mcx::MemoryContext;

    // PostgreSQL 18.6: CREATE TABLE t1 (id serial, a int); SELECT adbin FROM pg_attrdef.
    const SERIAL: &str = "{FUNCEXPR :funcid 480 :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 2 \
        :funccollid 0 :inputcollid 0 :args ({FUNCEXPR :funcid 1574 :funcresulttype 20 :funcretset false :funcvariadic false \
        :funcformat 0 :funccollid 0 :inputcollid 0 :args ({CONST :consttype 2205 :consttypmod -1 :constcollid 0 :constlen 4 \
        :constbyval true :constisnull false :location -1 :constvalue 4 [ 0 64 0 0 0 0 0 0 ]}) :location -1}) :location -1}";
    const SEVEN: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true \
        :constisnull false :location -1 :constvalue 4 [ 7 0 0 0 0 0 0 0 ]}";
    const EIGHT: &str = "{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true \
        :constisnull false :location -1 :constvalue 4 [ 8 0 0 0 0 0 0 0 ]}";

    fn trees(relid: types_core::Oid) -> usize {
        crate::with_state(|st| st.defaults_cache.get(&relid).map(|s| s.trees.borrow().as_ref().map_or(0, |v| v.len())))
            .unwrap_or(0)
    }

    #[test]
    fn default_read_once_copied_per_use_and_keyed_on_its_text() {
        let ctx = MemoryContext::new("t");
        let mcx = ctx.mcx();
        let text = |n| outfuncs::nodeToString(mcx, n).unwrap().as_str().to_string();
        let fresh = |s| text(readfuncs::stringToNode(mcx, s).unwrap());

        let a = super::RelationGetDefaultExpr(mcx, 41000, 1, SERIAL).unwrap();
        let b = super::RelationGetDefaultExpr(mcx, 41000, 1, SERIAL).unwrap();
        assert_eq!(trees(41000), 1);
        assert!(!a.ptr_eq(b));
        let set = crate::with_state(|st| st.defaults_cache.get(&41000).cloned()).unwrap();
        let cached = match set.lookup(1, SERIAL) {
            super::Lookup::Hit(t) => t,
            _ => panic!("the first use fills the set"),
        };
        assert!(!a.ptr_eq(cached) && !b.ptr_eq(cached));
        assert_eq!(text(a), fresh(SERIAL));
        assert_eq!(text(b), fresh(SERIAL));

        assert_eq!(text(super::RelationGetDefaultExpr(mcx, 41000, 2, SEVEN).unwrap()), fresh(SEVEN));
        assert_eq!(trees(41000), 2);
        assert!(std::rc::Rc::ptr_eq(&set, &crate::with_state(|st| st.defaults_cache.get(&41000).cloned()).unwrap()));

        // a column whose adbin changed under a set that was not dropped: never the old tree, a new set
        assert_eq!(text(super::RelationGetDefaultExpr(mcx, 41000, 2, EIGHT).unwrap()), fresh(EIGHT));
        assert_eq!(trees(41000), 1);
        assert!(matches!(set.lookup(2, SEVEN), super::Lookup::Hit(_)));
        drop(set);

        super::forget(41000);
        assert!(crate::with_state(|st| st.defaults_cache.get(&41000).is_none()));
        assert_eq!(text(super::RelationGetDefaultExpr(mcx, 41000, 1, SERIAL).unwrap()), fresh(SERIAL));
        assert_eq!(trees(41000), 1);

        assert!(super::RelationGetDefaultExpr(mcx, 41001, 1, "{NOSUCHNODE}").is_err());
        assert_eq!(trees(41001), 0);
    }

    // Born red on c2179497b3d, whose vector kept the address the context had before it moved into the Rc: the set's
    // first growth went through that dangling handle (box 24, 23:19:19Z: "memory allocation of 128 bytes failed").
    #[test]
    fn a_set_of_many_defaults_grows_and_reads_back() {
        let ctx = MemoryContext::new("t");
        let mcx = ctx.mcx();
        let text = |n| outfuncs::nodeToString(mcx, n).unwrap().as_str().to_string();
        let consts: Vec<String> = (0..9)
            .map(|k| {
                format!(
                    "{{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true \
                     :constisnull false :location -1 :constvalue 4 [ {k} 0 0 0 0 0 0 0 ]}}"
                )
            })
            .collect();
        for round in 0..2 {
            for (i, c) in consts.iter().enumerate() {
                let got = super::RelationGetDefaultExpr(mcx, 41002, i as i16 + 1, c).unwrap();
                assert_eq!(text(got), text(readfuncs::stringToNode(mcx, c).unwrap()), "round {round} column {}", i + 1);
            }
        }
        assert_eq!(trees(41002), 9);
    }

    #[test]
    fn the_tree_vector_allocates_in_the_sets_own_context() {
        let ctx = MemoryContext::new("t");
        for adnum in 1..=3 {
            super::RelationGetDefaultExpr(ctx.mcx(), 41003, adnum, SEVEN).unwrap();
        }
        let set = crate::with_state(|st| st.defaults_cache.get(&41003).cloned()).unwrap();
        let trees = set.trees.borrow();
        let v = trees.as_ref().expect("filled");
        assert!(core::ptr::eq(v.allocator().context(), &*set.cxt));
    }
}
