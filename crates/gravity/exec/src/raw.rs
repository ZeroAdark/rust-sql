//! The crate root's unsafe step: the admit context's handle for the backend's lifetime (`Backend::admit_mcx`).
#![allow(unsafe_code)]
use crate::Backend;

impl Backend {
    pub fn admit_mcx(&self) -> mcx::Mcx<'static> {
        // SAFETY: the admit context lives with the backend state and the admissions placed in it (`Admitted::keep`), dropped after
        unsafe { self.admit_ctx.held() }
    }
}

/// Memory step 3b: the executable's GNU build id (its `NT_GNU_BUILD_ID` note, up to 32 bytes) and its length; `None`
/// without one. Read from the executable's loaded program headers (the auxiliary vector's `AT_PHDR` / `AT_PHNUM`), never
/// from the file.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub(crate) fn build_id() -> Option<([u8; 32], usize)> {
    const NT_GNU_BUILD_ID: u32 = 3;
    // SAFETY: getauxval reads the process's auxiliary vector and nothing else.
    let (at, n) = unsafe { (libc::getauxval(libc::AT_PHDR) as usize, libc::getauxval(libc::AT_PHNUM) as usize) };
    if at == 0 || n == 0 {
        return None;
    }
    // SAFETY: AT_PHDR is the address of the executable's `AT_PHNUM` program headers, mapped for the process's life.
    let phs = unsafe { core::slice::from_raw_parts(at as *const libc::Elf64_Phdr, n) };
    // the load bias: where the headers are against the address PT_PHDR names (none: a fixed-address executable, bias 0)
    let bias = phs.iter().find(|p| p.p_type == libc::PT_PHDR).map_or(0, |p| at.wrapping_sub(p.p_vaddr as usize));
    for ph in phs.iter().filter(|p| p.p_type == libc::PT_NOTE) {
        // SAFETY: a PT_NOTE segment is mapped readable for its `p_memsz` bytes at the bias past its `p_vaddr`.
        let seg = unsafe { core::slice::from_raw_parts(bias.wrapping_add(ph.p_vaddr as usize) as *const u8, ph.p_memsz as usize) };
        let word = |at: usize| u32::from_ne_bytes([seg[at], seg[at + 1], seg[at + 2], seg[at + 3]]) as usize;
        let mut p = 0;
        while p + 12 <= seg.len() {
            let (namesz, descsz, kind) = (word(p), word(p + 4), word(p + 8) as u32);
            let (name, desc) = (p + 12, p + 12 + namesz.next_multiple_of(4));
            if desc + descsz > seg.len() {
                break;
            }
            if kind == NT_GNU_BUILD_ID && namesz == 4 && &seg[name..name + 4] == b"GNU\0" {
                let n = descsz.min(32);
                let mut id = [0u8; 32];
                id[..n].copy_from_slice(&seg[desc..desc + n]);
                return (n > 0).then_some((id, n));
            }
            p = desc + descsz.next_multiple_of(4);
        }
    }
    None
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub(crate) fn build_id() -> Option<([u8; 32], usize)> {
    None
}
