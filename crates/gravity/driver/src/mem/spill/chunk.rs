//! A hand-off chunk (`MemFile`'s, NEW-53): the records one hand-off's epochs wrote, shared by its file and the readers
//! open on it. Every block of it is charged to the file's owner by construction, in the one claim made with it: the
//! byte list's capacity and the shared block around it (the two counts and the `Chunk`). A growth past the reservation
//! re-fits the claim on the thread that grew it. The last holder to drop gives the charge back on its own thread (the
//! claim is declared first, so it drops first), where the frees then land.
use core::sync::atomic::AtomicUsize;
use std::sync::Arc;

use super::HeapClaim;

struct Chunk {
    claim: HeapClaim,
    bytes: Vec<u8>,
}

/// The shared block's bytes beside the byte list: the strong and weak counts and the `Chunk`.
const SHARED_BLOCK: u64 = (core::mem::size_of::<(AtomicUsize, AtomicUsize)>() + core::mem::size_of::<Chunk>()) as u64;

/// A holder of a hand-off chunk: the file's list, or a reader's extent.
pub(super) struct ChunkRef(Arc<Chunk>);

impl ChunkRef {
    /// A chunk with room for `bytes` record bytes, charged to `owner` with its shared block (`None`: uncharged).
    pub(super) fn new(owner: Option<memledger::Owner>, bytes: usize) -> ChunkRef {
        let v = Vec::with_capacity(bytes); // alloc: Exec charged owner
        let claim = HeapClaim::of(owner, memledger::Kind::Spill, &v).beside(SHARED_BLOCK);
        ChunkRef(Arc::new(Chunk { claim, bytes: v })) // alloc: Exec charged owner
    }

    /// Another holder of the same chunk (a reader's): a count, no copy.
    pub(super) fn share(&self) -> ChunkRef {
        ChunkRef(Arc::clone(&self.0))
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }

    /// The chunk to append to, when no reader shares it (a shared chunk is never written again).
    pub(super) fn get_mut(&mut self) -> Option<ChunkMut<'_>> {
        Arc::get_mut(&mut self.0).map(ChunkMut)
    }
}

/// A chunk no reader shares, for its file's epoch write.
pub(super) struct ChunkMut<'a>(&'a mut Chunk);

impl ChunkMut<'_> {
    /// `b` appended; its offset in the chunk. A growth past the reservation (an epoch with no hand-off's reserve before
    /// it) is charged at its new capacity at once.
    pub(super) fn append(&mut self, b: &[u8]) -> usize {
        let Chunk { claim, bytes } = &mut *self.0;
        let (off, cap) = (bytes.len(), bytes.capacity());
        bytes.extend_from_slice(b); // alloc: Exec charged claim
        if bytes.capacity() != cap {
            claim.fit(bytes);
        }
        off
    }
}
