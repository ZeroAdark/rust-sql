//! D3267: the hosts' mapped road, a host's bytes past its operator's work_mem carved from temp files mapped shared (D491's
//! road, `gravity_driver::map_temp`: temp_file_limit charged, unlinked once mapped, written back by the kernel under memory
//! pressure as PostgreSQL's temp files are). Any thread reads what another wrote (a pool worker's carve, read by the leader
//! after the section), which a tuplesort's or a BufFile's temp file cannot give: those live in the creating thread's VFD
//! table. The DISTINCT run's chunks and images past work_mem take it (ops/group/raw/distinct.rs). A home: it imports no
//! family.
use gravity_abi::limits::{MAPPED_ROAD_MAX_BYTES, MAPPED_ROAD_MIN_BYTES};
use gravity_driver::TempMap;
use types_error::{PgError, PgResult};

/// One owner's mappings (a hosted aggregate slot's) and the cursor in the newest; dropping it unmaps them all, so the owner
/// drops it only when nothing points into them (the slot's aggregate context reset or dropped).
#[derive(Default)]
pub(crate) struct Road {
    maps: Vec<TempMap>, // alloc: Exec
    at: usize,
    end: usize,
    mapped: usize,
}

impl Road {
    /// `bytes` (rounded up to 8) from the newest mapping, or a new mapping when it has no room; `None` when the session has
    /// no file road (the caller keeps its memory context). An error is the file's own (53400 past temp_file_limit).
    pub(crate) fn carve(&mut self, bytes: usize) -> PgResult<Option<*mut u8>> {
        let bytes = bytes.next_multiple_of(8);
        if self.end - self.at < bytes {
            let want = self.mapped.clamp(MAPPED_ROAD_MIN_BYTES, MAPPED_ROAD_MAX_BYTES).max(bytes);
            let m = match gravity_driver::map_temp(want) {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(None),
                Err(_) => return Err(crate::take_error().unwrap_or_else(|| Box::new(PgError::error(String::from("gravity: a temp file map for the mapped road failed"))))), // alloc: Cold
            };
            self.mapped += m.len();
            (self.at, self.end) = (m.as_mut_ptr() as usize, m.as_mut_ptr() as usize + m.len());
            self.maps.push(m); // alloc: Exec
        }
        let p = self.at as *mut u8;
        self.at += bytes;
        Ok(Some(p))
    }

    /// Whether the owner already took the road (a mapping is held).
    pub(crate) fn on(&self) -> bool {
        !self.maps.is_empty()
    }
}
