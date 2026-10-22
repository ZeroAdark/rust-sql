//! The `SubscriptingRef` fetch (expr.md §7): the container, then every provided subscript where the container is
//! not null (C jumps over the subscripts' steps on a null container and evaluates all of them otherwise), then the
//! node's site — strict, so a null container or subscript is a null result; an assignment is emit/store.rs's.

use gravity_ir::Type;
use mcx::GrowIn;

use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::subscript::{words, Subscripting};
use crate::nodes::NodeId;

impl Em<'_, '_, '_> {
    pub(super) fn subscript(&mut self, n: &Subscripting) -> Result<Lowered, EmitError> {
        if n.assign.is_some() {
            return self.store(n);
        }
        let cur = self.node(n.container)?;
        let mut args = mcx::reserved_in(3 + (n.upper.len + n.lower.len) as usize, self.row.scratch);
        for w in words(self.p, n) {
            let value = self.b.iconst(Type::I64, w);
            args.push_in(Lowered { value, valid: self.t, err: self.zero });
        }
        args.push_in(Lowered { value: cur.value, valid: cur.valid, err: cur.err });
        // D775: a subscript evaluated only where nothing before it raised; the first raise kept (`call` takes `first`)
        let mut err = cur.err;
        for list in [n.upper, n.lower] {
            for i in 0..list.len as usize {
                let id = self.p.kids[list.start as usize + i];
                if id != NodeId::NONE {
                    let go = self.go_fresh(cur.valid, err);
                    let s = self.guarded(go, id)?;
                    err = self.first(err, s.err);
                    args.push_in(s);
                }
            }
        }
        self.call(n.site, &args, false)
    }
}
