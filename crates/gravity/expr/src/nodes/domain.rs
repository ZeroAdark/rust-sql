//! Domain checks for callers outside an expression (the tail lane's file, expr-compiler.md
//! §4.2): `Check::compile(domain)` compiles the constraints once, `check(value)` raises
//! `23502` / `23514` as `CoerceToDomain` does (`compat`'s `domain_check_input` for a bare value).

use gravity_fn::{NullableDatum, Region};
use types_core::Oid;
use types_error::PgResult;
use types_portal::params::ParamBind;

use crate::catalog::{Catalog, DomainConstraint};
use crate::eval::{Cx, Row};
use crate::nodes::{Builder, NodeId, Program, State};
use crate::CompileResult;

pub struct Check<'mcx> {
    pub typ: Oid,
    pub notnull: bool,
    prog: Program<'mcx>,
    state: State,
    /// Each CHECK's node and its name's index in `prog.names` (the program's copy: the type cache frees a replaced set).
    checks: Vec<(NodeId, u32)>,
    params: ParamBind<'mcx>,
}

impl<'mcx> Check<'mcx> {
    /// The checks compiled once, the program's lists in `m` (memsig E-prog: `Program::empty_in`).
    pub fn compile(m: mcx::Mcx<'mcx>, typ: Oid, catalog: &dyn Catalog<'mcx>) -> CompileResult<Check<'mcx>> {
        let facts = catalog.domain(typ)?;
        let mut prog = Program::empty_in(m);
        let mut state = State::empty();
        let mut checks = Vec::new(); // alloc: Program
        let mut notnull = false;
        {
            let mut b = Builder { prog: &mut prog, state: &mut state, catalog, outer: None, tableoid: None, aggorder: false, returning: None, folded: false };
            for con in &facts.constraints {
                match con {
                    DomainConstraint::NotNull => notnull = true,
                    DomainConstraint::Check { name, expr } => {
                        let id = b.compile(*expr)?;
                        checks.push((id, b.name(name)));
                    }
                }
            }
        }
        Ok(Check { typ, notnull, prog, state, checks, params: ParamBind::NONE })
    }

    /// The value against the domain: `Ok(())` or the violation as `CoerceToDomain` raises it.
    pub fn check(&mut self, value: NullableDatum, region: &mut Region) -> PgResult<()> {
        if self.notnull && value.isnull {
            return Err(crate::nodes::cast::domain_null(self.typ));
        }
        let row = Row::EMPTY;
        let mut cx = Cx::new(&row, region, &self.params, core::ptr::null_mut());
        cx.domainval = value;
        let (prog, mut state) = (self.prog.view(), self.state.view());
        for &(id, name) in &self.checks {
            let c = crate::eval::eval(&prog, &mut state, id, &mut cx)?;
            if !c.isnull && !c.value.as_bool() {
                return Err(crate::nodes::cast::domain_check(self.typ, prog.names[name as usize]));
            }
        }
        Ok(())
    }

    pub fn reset_window(&mut self) {
        self.state.reset_window();
    }
}
