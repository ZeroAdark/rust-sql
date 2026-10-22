//! Hand adapters over the array bodies: `array_dims` formats the bounds into a stack buffer
//! (the wrapper's temporary `String`) and copies the text into the region.

use core::fmt::Write;

use arrayfuncs::foundation::{read_dims_lbounds, MAXDIM};

use crate::{null_ret, region_payload, ErrSlot, Region, Ret, View};

struct Buf {
    bytes: [u8; 24 * MAXDIM],
    len: usize,
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

pub extern "C-unwind" fn array_dims(_e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    let (ndim, dims, lbs) = read_dims_lbounds(a0.bytes());
    if ndim <= 0 || ndim > MAXDIM as i32 {
        return null_ret();
    }
    let mut buf = Buf { bytes: [0; 24 * MAXDIM], len: 0 };
    for i in 0..ndim as usize {
        write!(buf, "[{}:{}]", lbs[i], dims[i] + lbs[i] - 1).unwrap();
    }
    region_payload(r, &buf.bytes[..buf.len])
}
