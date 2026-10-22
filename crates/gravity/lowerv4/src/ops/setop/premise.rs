//! setop's premises (v5 landing 4, moved from props/ops.rs's set-op arms): each key's hash kind, by its input column's
//! type. The props walk calls `setop` for each Op::SetOp.
use gravity_decompose::ops::shared::setop::SetDesc;
use types_nodes::list::NodeList;

use crate::applies::Prop;
use crate::props::code::entry;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::SetOp (`input`: the projection before it, if any).
    pub(crate) fn setop(&mut self, d: &SetDesc<'p>, input: Option<&'p NodeList<'p>>) {
        let packed = self.p.has(Prop::Packed);
        let (SetDesc::Identity { keys } | SetDesc::Set { keys, .. }) = d else { return };
        // a key's arm by its input column's type, as a window key's (a text key is varlena, never hosted)
        let tlist = if let SetDesc::Set { tlist, .. } = d { Some(*tlist) } else { None };
        for key in keys.iter() {
            let e = input.and_then(|l| entry(l, key.resno)).or_else(|| tlist.and_then(|l| entry(l, key.resno)));
            let image = e.and_then(|e| self.image_of(gravity_decompose::catalog::expr_type::type_of(e)?));
            self.add(key_kind(key.kind, image, packed, Accessor::Hash));
        }
    }
}
