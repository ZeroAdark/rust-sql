//! CstrPtr datum (plan §3.2; KEEP lowerv3 rep/cstr.rs). A cstring (`typlen -2`: `cstring`, `unknown` literals, a C-string
//! result) reads through the varlena datum arm (`rep::datum_kind`: its datum is its pointer, as a varlena's), so this file
//! registers no kind; it holds what differs from a varlena: the size a retain copies, `strlen + 1` (the terminator kept,
//! as `datumCopy` of a `typlen -2` datum).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, Type, Value};

/// The bytes a cstring's copy takes: its length and the terminating NUL. Entered only for a non-NULL pointer.
pub(crate) fn size(b: &mut FunctionBuilder<'_>, p: Value) -> Value {
    let h = b.declare_host_once("rep_cstr_size", &[Type::I64], Some(Type::I64), cstr_size as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    b.call_host(h, &[p], Some(Type::I64)).expect("returns the size")
}

use super::raw::datum::cstr_size;

#[cfg(test)]
mod tests {
    use gravity_program::Rep;

    #[test]
    fn a_cstring_is_kept_with_its_terminator() {
        let src = b"pgrust\0tail";
        let mut dst = [0xAAu8; 16];
        let v = crate::rep::varlena::datum::tests::kept(Rep::CstrPtr, 2275, src, &mut dst);
        assert_eq!(v, dst.as_ptr() as u64);
        assert_eq!(&dst[..7], b"pgrust\0");
        assert_eq!(dst[7], 0xAA, "nothing past the terminator");
    }
}
