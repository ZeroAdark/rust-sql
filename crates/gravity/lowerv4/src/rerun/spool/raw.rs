//! nest.spool's raw half (rule R6): the record's one direct host, `spool_keep`, which copies a by-reference datum into the
//! spool object's arena (rec.rs `Rec::store` calls it through the IR), written through the contract's `host!`.
#![allow(unsafe_code)]
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_kernels::ops::shared::object::entries as m8;

/// PostgreSQL's `VARTAG_ONDISK` (varatt.h).
const VARTAG_ONDISK: u8 = 18;

gravity_abi::host! {
    /// The by-reference datum at `datum` (`typlen` > 0 fixed, -1 varlena, -2 cstring) copied into spool `h`'s arena: its
    /// address, or `ERROR` with the error stashed. An expanded or indirect varlena is flattened (a 4-byte header then the
    /// payload); an on-disk external pointer or a compressed inline value is copied as it is (PostgreSQL's tuplestore keeps
    /// both); the flattened copy is charged to `ctx`'s owner for its life (rep/varlena `value`, CLASS E).
    /// # Safety — `datum` is a live datum of that length form; `h` a live spool cell.
    pub(crate) unsafe fn spool_keep(ctx: u64, h: u64, datum: u64, typlen: i64) -> i64 {
        let word = ctx;
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let p = datum as usize as *const u8;
        let len = match typlen {
            n if n > 0 => n as usize,
            -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes_with_nul().len(),
            _ if *p == 1 && *p.add(1) != VARTAG_ONDISK => {
                let flat = gravity_fn::with_payload(p, |x| {
                    let v = crate::rep::varlena::raw::value::copy_of(word, &[&(((4 + x.len()) as u32) << 2).to_ne_bytes()[..], x]);
                    m8::m8_spool_copy(ctx, h, v.as_ptr(), v.len() as u64)
                });
                return flat.unwrap_or_else(|e| {
                    gravity_fn::stash_error(e);
                    ERROR
                });
            }
            // VARSIZE_ANY: an on-disk external pointer (the header byte, its tag, 16 bytes), a short header, a 4-byte one
            _ if *p == 1 => 18,
            _ if *p & 1 == 1 => (*p >> 1) as usize,
            _ => (p.cast::<u32>().read_unaligned() >> 2) as usize,
        };
        m8::m8_spool_copy(ctx, h, p, len as u64)
    }
}
