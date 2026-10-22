//! a BufFile closed on a thread that did not open it.
//!
//! A BufFile's Files are indices into the VFD table of the thread that opened them (vfd.rs: the table is a thread-local).
//! Its close must run there. On another thread each `FileClose` frees whatever that thread holds at the same index: on
//! a pool thread, an md segment, which md then reads through the slot's next file (XX001 short reads past that file's
//! end, another relation's pages inside it). A Gravity co-split claim's run readers are given up by the same worker's
//! next claim, which the runtime may run on another pool thread, and a node reset drops every worker's readers on the
//! leader.
//!
//! So the close goes back to the owner: `send` queues the files, with each slot's generation, in the owner's inbox,
//! and the owner closes them at its next file open (`drain`), each only while its slot still holds that open
//! (`FileCloseGeneration`). The inbox lives in the owner's fd state (`FdState::orphan`, made at its first BufFile): an
//! owner that exits drops it with its table, whose drop closes the descriptors, and what is sent to it after that is
//! dropped unclosed.

use std::sync::Arc;

use pgsync::atomic::{AtomicBool, Ordering};
use pgsync::thread::ThreadId;
use types_storage::File;

use crate::vfd::{fd_tls_alive, with_fd};

struct Inbox {
    pending: AtomicBool,
    files: pgsync::Mutex<Vec<(File, u64)>>,
}

static OWNERS: pgsync::Mutex<Vec<(ThreadId, Arc<Inbox>)>> = pgsync::Mutex::new(Vec::new());

pub(crate) struct Registered {
    id: ThreadId,
    inbox: Arc<Inbox>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        pgsync::lock(&OWNERS).retain(|(id, _)| *id != self.id);
    }
}

pub(crate) fn owner() -> Option<ThreadId> {
    if !fd_tls_alive() {
        return None;
    }
    Some(with_fd(|fd| {
        if let Some(r) = &fd.orphan {
            return r.id;
        }
        let id = pgsync::thread::current().id();
        let inbox = Arc::new(Inbox { pending: AtomicBool::new(false), files: pgsync::Mutex::new(Vec::new()) });
        pgsync::lock(&OWNERS).push((id, Arc::clone(&inbox)));
        fd.orphan = Some(Registered { id, inbox });
        id
    }))
}

pub(crate) fn is_current(owner: ThreadId) -> bool {
    fd_tls_alive() && with_fd(|fd| fd.orphan.as_ref().is_some_and(|r| r.id == owner))
}

pub(crate) fn send(to: ThreadId, files: impl IntoIterator<Item = (File, u64)>) {
    let inbox = pgsync::lock(&OWNERS).iter().find(|(id, _)| *id == to).map(|(_, inbox)| Arc::clone(inbox));
    if let Some(inbox) = inbox {
        pgsync::lock(&inbox.files).extend(files);
        inbox.pending.store(true, Ordering::Release);
    }
}

pub fn drain() {
    if !fd_tls_alive() {
        return;
    }
    let files = with_fd(|fd| {
        fd.orphan
            .as_ref()
            .filter(|r| r.inbox.pending.load(Ordering::Relaxed) && r.inbox.pending.swap(false, Ordering::AcqRel))
            .map(|r| core::mem::take(&mut *pgsync::lock(&r.inbox.files)))
    });
    for (file, gen) in files.into_iter().flatten() {
        let _ = crate::io::FileCloseGeneration(file, gen);
    }
}

#[cfg(test)]
pub(crate) fn pending() -> bool {
    with_fd(|fd| fd.orphan.as_ref().is_some_and(|r| r.inbox.pending.load(Ordering::Acquire)))
}
