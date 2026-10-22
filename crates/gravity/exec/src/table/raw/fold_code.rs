//! The owner fold's compiler: compiled through the process code cache, so its bytes are charged as every filed module's.
use gravity_kernels::table::fold_jit::{Compiled, Made};
use gravity_kernels::table::owner_fold::Fold;

/// `Later` with the code cache off: a module it does not file is its compiling statement's, never the registry's.
pub fn compile(f: &Fold) -> Made {
    if gravity_backend::cache::budget_bytes() == 0 {
        return Made::Later;
    }
    let mut pb = gravity_backend::gravity_ir::builder::ProgramBuilder::new();
    let Some(e) = gravity_lowerv4::table::owner_fold::emit(&mut pb, f) else { return Made::Refused };
    let p = pb.finish();
    match gravity_backend::compiled(&p, e) {
        Ok(_) if gravity_backend::cache::budget_bytes() == 0 => Made::Later,
        Ok((code, _)) => {
            let (entry, bytes) = (code.entry(), code.bytes());
            match code.into_owner() {
                gravity_program::Owner::Mapped(own) => Made::Code(Compiled { entry, bytes, own }),
                _ => Made::Refused,
            }
        }
        Err(e) => {
            let class = types_error::error::errcode_to_category(e.sqlstate());
            let transient = [types_error::error::ERRCODE_OUT_OF_MEMORY, types_error::error::ERRCODE_QUERY_CANCELED].iter().any(|&c| types_error::error::errcode_to_category(c) == class);
            if transient || e.message() == gravity_backend::cache::INTERRUPTED { Made::Later } else { Made::Refused }
        }
    }
}
