//! `scratch`'s unsafe steps (rule R6, the module's raw home): a host's bump contexts, handed out for the host's life.
#![allow(unsafe_code)]

use mcx::Mcx;

use super::NodeScratch;

impl NodeScratch {
    pub fn mcx(&self) -> Mcx<'static> {
        // SAFETY: the objects placed in it are fields of the same host, declared before `scratch`, so they drop
        // before this root does.
        unsafe { self.node.held() }
    }

    pub fn exec_mcx(&self) -> Mcx<'static> {
        // SAFETY: as `mcx`; reset at unbind, and by a bitmap source at its seek (D2970), when nothing references it.
        unsafe { self.exec.held() }
    }

    /// The per-bind context (`with_bind`), else `node`'s.
    pub fn bind_mcx(&self) -> Mcx<'static> {
        match self.bind.as_ref() {
            // SAFETY: as `mcx`; reset at unbind (`reset_bind`) after the scans placed in it ended, when nothing references it.
            Some(b) => unsafe { b.held() },
            None => self.mcx(),
        }
    }
}
