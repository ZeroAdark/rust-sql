//! jsonb shredded (expr-compiler.md §2.7): a `->` / `->>` chain over a jsonb column with constant
//! keys, or a `CoerceViaIO` of one, whose `(column, path)` the row references name as a shredded
//! lane is that lane's load (the validity bit = the key present); no lane, the value form.

use gravity_fn::Datum;
use types_core::Oid;
use types_tuple::varatt;

use super::LaneRef;
use crate::eval::Source;
use crate::nodes::{ExprNode, NodeId, Program};

pub const F_JSONB_OBJECT_FIELD: Oid = 3478;
pub const F_JSONB_OBJECT_FIELD_TEXT: Oid = 3214;
pub const JSONBOID: Oid = 3802;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShredType {
    Text,
    Fixed(Oid),
}

#[derive(Debug)]
pub struct ShredRef {
    pub attno: i16,
    pub path: Vec<Vec<u8>>,
    pub lane: LaneRef,
    pub typ: ShredType,
}

/// The chain `Var -> k1 -> k2 ->> kn` under `id`: the column and the keys, and whether the last
/// step is `->>` (text); `None` for anything else.
pub fn path_of(p: &Program<'_>, id: NodeId) -> Option<(i16, Vec<Vec<u8>>, bool)> {
    let mut keys: Vec<Vec<u8>> = Vec::new(); // alloc: Program
    let mut cur = id;
    let mut text = None;
    loop {
        match &p.nodes[cur.0 as usize] {
            ExprNode::Relabel(r) => cur = r.arg,
            ExprNode::Op(n) | ExprNode::Func(n) => {
                let call = &p.sites[n.site.0 as usize];
                let is_text = match call.funcid {
                    F_JSONB_OBJECT_FIELD_TEXT => true,
                    F_JSONB_OBJECT_FIELD => false,
                    _ => return None,
                };
                if call.args.len != 2 || (text.is_none() && !is_text) || (text.is_some() && is_text) {
                    return None;
                }
                text.get_or_insert(is_text);
                let (obj, key) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
                let ExprNode::Const(c) = &p.nodes[key.0 as usize] else { return None };
                if c.is_null() || c.typlen != -1 {
                    return None;
                }
                keys.push(text_bytes(c.read().value));
                cur = obj;
            }
            ExprNode::Var(v) if v.src == Source::Scan && v.typ == JSONBOID && !keys.is_empty() => {
                keys.reverse();
                return Some((v.attno, keys, text == Some(true)));
            }
            _ => return None,
        }
    }
}

fn text_bytes(d: Datum) -> Vec<u8> {
    let ptr = d.as_usize() as *const u8;
    // SAFETY: a non-null text constant is a live varlena image the plan owns.
    unsafe {
        if varatt::varatt_is_1b(ptr) {
            core::slice::from_raw_parts(ptr.add(1), varatt::varsize_1b(ptr) - 1).to_vec() // alloc: Program
        } else {
            core::slice::from_raw_parts(ptr.add(4), varatt::varsize_4b(ptr) - 4).to_vec() // alloc: Program
        }
    }
}

pub fn find<'a>(shreds: &'a [ShredRef], attno: i16, path: &[Vec<u8>], typ: ShredType) -> Option<&'a ShredRef> {
    shreds.iter().find(|s| s.attno == attno && s.path == path && s.typ == typ)
}
