//! The `Code` shape of a wasm program: module bytes plus an instance handle
//! owned by an embedder, carried by `Owner::Interp`'s `run(state, ctx)` (crate::raw).

use gravity_abi::Ctx;
use gravity_program::Code;
use std::sync::Arc;

pub type Handle = u32;

pub trait Embedder: Send + Sync {
    fn instantiate(&self, bytes: &[u8]) -> Result<Handle, String>;
    fn call(&self, handle: Handle, ctx: u32) -> i64;
    fn release(&self, handle: Handle);
}

pub struct WasmCode {
    bytes: Vec<u8>,
    handle: Handle,
    embedder: Arc<dyn Embedder>,
}

impl WasmCode {
    pub fn new(embedder: Arc<dyn Embedder>, bytes: Vec<u8>) -> Result<WasmCode, String> {
        let handle = embedder.instantiate(&bytes)?;
        Ok(WasmCode { bytes, handle, embedder })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn handle(&self) -> Handle {
        self.handle
    }

    /// One call of the instance on `ctx` (its address as the module's 32-bit linear-memory pointer).
    #[inline]
    pub(crate) fn call(&self, ctx: *mut Ctx) -> i64 {
        self.embedder.call(self.handle, ctx as usize as u32)
    }

    /// Its identity is an FNV-1a 128 of the module bytes (D303: every interpreted entry is one function).
    pub fn into_code(self) -> Code {
        let mut id: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        for &b in self.bytes.iter() {
            id = (id ^ b as u128).wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        }
        Code::interp(crate::raw::run, Box::new(self)).with_identity(id)
    }
}

impl Drop for WasmCode {
    fn drop(&mut self) {
        self.embedder.release(self.handle);
    }
}
