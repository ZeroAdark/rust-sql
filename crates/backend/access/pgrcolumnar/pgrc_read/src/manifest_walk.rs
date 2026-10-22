//! Effective-manifest resolution (spec §13): read `CURRENT` (the O(1)
//! candidate hint), then walk `prev_gen` past non-committed generations —
//! the clog fence (#254). Effectiveness is a commit fact, so the clog
//! consult arrives through the [`CommitCheck`] seam (tests model publishers directly).
//!
//! Crash-consistency consequences of the §13.3 publish ordering, encoded here as behavior:
//!
//! - **Absent `CURRENT` ⇒ empty table** (`Ok(None)`): the commit record is
//!   written only AFTER `CURRENT` publishes and the directory fsyncs, so no
//!   committed publish can exist without it. No directory scanning fallback
//!   exists — a scan could only find generations that are structurally
//!   invisible.
//! - **A corrupt `CURRENT` or a missing/corrupt chained manifest is a typed
//!   refusal**, never silent emptiness: COMMITTED generations were fsynced
//!   before any commit that references them, so on a RECOVERED directory
//!   damage is real corruption. Un-recovered crash residue can legally show
//!   a dangling `CURRENT` (a pre-commit dirent-loss window can persist the
//!   `CURRENT` rename while dropping the `manifest-<g>` link); the #462
//!   repoint law makes end-of-recovery cleanup — which runs before readers —
//!   repair exactly that, so the refusal here stays a corruption tripwire.
//!   Scan fallback is a writer-side affordance only (spec §13.3): a reader
//!   cannot distinguish missing uncommitted residue from a missing
//!   committed manifest, so softening this walk would swallow corruption.

use pgrc_format::dirlayout::{manifest_file_name, CURRENT_FILE_NAME};
use pgrc_format::manifest::{CommitPointer, Manifest};
use pgrc_format::sections::{decode_front, read_front_bytes};
use pgrc_format::wire::crc32c;
use pgrc_format::FormatError;

use crate::io::TableDirIo;
use crate::{ReadError, ReadResult};

/// The clog seam: is the publishing FullTransactionId (epoch-qualified: no xid aliasing, spec §13.1) committed?
pub trait CommitCheck {
    fn committed(&self, fxid: u64) -> bool;

    /// D339 (trunk 0a14b58f683, ported): for a generation whose publisher `committed` or `own` accepted, does the
    /// check's snapshot see it at the command grain? Only a generation the current transaction published can be unseen
    /// here (a cursor's snapshot taken before the publishing COPY: heap's cmin < curcid rule).
    /// `key` = `(spc, db, relfilenumber, gen)`. Default: yes.
    fn sees_command(&self, _fxid: u64, _key: (u32, u32, u64, u64)) -> bool {
        true
    }

    /// RR-VIEW: the publisher is the reader's own transaction AND the check is an MVCC snapshot's, so the reader
    /// composes its own view (`pgrc_am::catalog::visible`). Default: never, so the newest committed-or-own generation
    /// is read whole (no snapshot: writers, planner facts).
    fn own(&self, _fxid: u64) -> bool {
        false
    }
}

/// Every publisher committed — tools/tests over known-good directories.
pub struct AllCommitted;

impl CommitCheck for AllCommitted {
    fn committed(&self, _fxid: u64) -> bool {
        true
    }
}

/// Validation facts for the table the directory must belong to.
#[derive(Debug, Clone, Copy, Default)]
pub struct TableExpect {
    pub relfilenumber: Option<u64>,
    pub schema_fingerprint: Option<u64>,
}

/// The resolved effective generation.
#[derive(Debug, Clone)]
pub struct EffectiveManifest {
    pub manifest: Manifest,
    /// Generations walked past (non-committed publishers) before this one; 0 when `CURRENT`'s was effective.
    pub walked_past: u32,
}

/// The effective manifest generation of a table directory; `Ok(None)` = no committed publish (empty table).
pub fn resolve_effective(
    dir: &dyn TableDirIo,
    check: &dyn CommitCheck,
    expect: &TableExpect,
) -> ReadResult<Option<EffectiveManifest>> {
    let Some(cur_bytes) = dir.read_file(CURRENT_FILE_NAME)? else {
        return Ok(None);
    };
    let ptr = CommitPointer::decode(&cur_bytes)?;
    let mut gen = ptr.gen;
    let mut walked_past = 0u32;
    // a manifest read a tick: the walk goes back past uncommitted generations only, one read each
    let mut step = cancelpoll::step("pgrc_read resolve_effective: a manifest generation's read");
    loop {
        step.tick()?;
        let (m, s0, bytes) = read_front(dir, gen)?;
        // The commit pointer pins section 0's length and trailing CRC
        // (spec §13.2) — checked for the candidate generation only.
        if gen == ptr.gen {
            if s0 as u64 != ptr.manifest_len {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "CommitPointer manifest_len",
                }));
            }
            let trailing = crc32c(&bytes[..s0 - 4]);
            if trailing != ptr.manifest_crc {
                return Err(ReadError::Format(FormatError::CrcMismatch {
                    at: "CommitPointer manifest_crc",
                }));
            }
        }
        if m.header.gen != gen {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "manifest gen vs file name",
            }));
        }
        validate_table_facts(&m, expect)?;
        if check.committed(m.header.publisher_fxid) {
            return Ok(Some(EffectiveManifest {
                manifest: m,
                walked_past,
            }));
        }
        if m.header.prev_gen == 0 {
            // Every generation back to the first is uncommitted: the table
            // has no effective content.
            return Ok(None);
        }
        gen = m.header.prev_gen;
        walked_past += 1;
    }
}

/// Section 0 of `manifest-<gen>` (`read_front_bytes`): the manifest, section 0's length and the bytes read.
pub fn read_front(dir: &dyn TableDirIo, gen: u64) -> ReadResult<(Manifest, usize, Vec<u8>)> {
    let name = manifest_file_name(gen);
    let (bytes, _) = read_front_bytes(&mut |len| dir.read_prefix(&name, len)?.ok_or(ReadError::ManifestMissing { gen }))?;
    let (m, s0) = decode_front(&bytes)?;
    Ok((m, s0, bytes))
}

fn validate_table_facts(m: &Manifest, expect: &TableExpect) -> ReadResult<()> {
    if let Some(want) = expect.relfilenumber {
        if m.header.relfilenumber != want {
            return Err(ReadError::OpenMismatch {
                field: "manifest relfilenumber",
            });
        }
    }
    if let Some(want) = expect.schema_fingerprint {
        if m.header.schema_fingerprint != want {
            return Err(ReadError::OpenMismatch {
                field: "manifest schema_fingerprint",
            });
        }
    }
    Ok(())
}
