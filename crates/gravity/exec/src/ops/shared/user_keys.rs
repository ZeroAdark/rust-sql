//! `keys.user` (D4197): the run's user-key classes (kernels typesem/userfp.rs `RunKeys`), made per execution on the thread
//! that runs it, its handle laid into the program's word, and dropped with the execution's protocols on every path (an
//! error's end too). Every pipeline of the execution reads the same word, so the same classes: an InitPlan's, a rerun
//! set's, a LAZY hashed set's, a cursor's later FETCH and its rewind; a nested statement is an execution of its own.
use gravity_kernels::typesem::userfp::RunKeys;
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use crate::glue;
use crate::ops::{Live, Prepare, Protocol};

pub const PROTOCOLS: &[&dyn Protocol] = &[&UserKeys];

pub struct UserKeys;

/// One execution's classes, freed when its protocols drop.
struct Held {
    _keys: RunKeys,
}

impl Protocol for UserKeys {
    fn kind(&self) -> ProtoKind {
        ProtoKind::UserKeys
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        let keys = RunKeys::new();
        // CONVERT wave 2 (site 13): the run's classes are the statement's (every pipeline of the execution reads them)
        keys.own(gravity_driver::stmt_context().map(|(o, _)| (o, gravity_fn::arena::Kind::Region)));
        match (p.words.n, cx.words.get_mut(base), cx.nulls.get_mut(base)) {
            (1, Some(w), Some(z)) => (*w, *z) = (keys.handle(), 0),
            _ => return Err(glue::internal("gravity: keys.user without its handle word")),
        }
        *cx.nparams = (*cx.nparams).max(base + 1);
        Ok(Some(Box::new(Held { _keys: keys }))) // alloc: Exec
    }
}

impl Live for Held {
    fn answers(&self) -> u16 {
        crate::ops::ask::NONE
    }
}
