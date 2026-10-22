//! The program dump's unsafe step (rule R6, its raw home), moved from dump.rs: the mapped code read as bytes.
#![allow(unsafe_code)]

/// The mapped code written to `path` byte for byte (an empty file when nothing is mapped): its base address and length.
pub(super) fn write_mapped(path: &std::path::Path, compiled: &gravity_tpde::Compiled<'_>) -> std::io::Result<(usize, usize)> {
    let range = compiled.mapper().get_mapped_range();
    let (base, bytes): (usize, &[u8]) = match &range {
        // SAFETY: the mapping lives as long as `compiled`; it is read, never executed or written, here.
        Some(r) => (r.address(), unsafe { r.bytes() }),
        None => (0, &[]),
    };
    std::fs::write(path, bytes)?;
    Ok((base, bytes.len()))
}
