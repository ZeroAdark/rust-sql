//! Memory step 3b: the memory history (`gravity_driver::mem::feedback`) kept across restarts in one cluster file,
//! `pg_stat/pgrust_gravity_grant_feedback`: under `pg_stat/`, which pg_checksums never scans (pg_stat_statements' place).
//! Loaded whole at the first admission under `pgrust.gravity_grant_feedback` after a start; written at the end of each
//! checkpoint and restartpoint and at the shutdown checkpoint (`flush`, the checkpointer's `gravity_entry_seams` call),
//! never WAL-logged: a crash loses the runs since the last checkpoint, never the file's integrity. A file another binary
//! (its GNU build id) or another cluster (its pg_control system identifier) wrote, one of another format version, or a torn
//! or corrupt one, is ignored with a LOG line and rewritten at the next flush. Off (never loaded since the start) loads,
//! writes and creates nothing.

use gravity_driver::mem::feedback;

const FILE: &str = "pg_stat/pgrust_gravity_grant_feedback";
const MAGIC: [u8; 8] = *b"PGRGFBK\0";
/// 2: the cluster is named by its system identifier (version 1 named it by a fingerprint seed, now drawn per process).
const VERSION: u32 = 2;
/// magic, version, the build id (32, zero-padded) and its length, the system identifier, the entry count, the body's bytes,
/// CRC-32C.
const HEADER: usize = 8 + 4 + 32 + 4 + 8 + 4 + 4 + 4;

/// What a file must carry to be this process's: the executable's build id and the cluster's system identifier.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Ident {
    build: [u8; 32],
    build_len: u32,
    system: u64,
}

/// `None`: no build id, or no control file read (a rig): the history is not kept across restarts.
fn ident() -> Option<Ident> {
    let (build, n) = crate::raw::build_id()?;
    Some(Ident { build, build_len: n as u32, system: gravity_hosts::inputs::system_identifier()? })
}

/// Set at the first admission under feedback: whether this process keeps the file (an identity and a data directory).
static KEEPS: pgsync::OnceLock<bool> = pgsync::OnceLock::new();

fn log(msg: String) {
    let _ = elog::ereport(types_error::LOG).errmsg(msg).finish(types_error::ErrorLocation::new("gravity", 0, "feedback"));
}

fn path() -> Option<String> {
    init_small::globals::DataDir().map(|dd| format!("{dd}/{FILE}")) // alloc: Cold
}

/// The first admission under feedback since the start: the file read into the history, once per process.
pub(crate) fn load_once() {
    KEEPS.get_or_init(load);
}

#[cold]
fn load() -> bool {
    let Some((path, id)) = path().and_then(|p| Some((p, ident()?))) else {
        log("gravity: grant feedback: no data directory, build id or control file; the history is not kept across restarts".to_string()); // alloc: Cold
        return false;
    };
    let bytes = match fd::sync::read_whole_file(&path) {
        Ok(b) => b,
        Err(_) => return true,
    };
    match parse(&bytes, &id).and_then(|(n, body)| feedback::import(body, feedback::now()).map_err(|()| "an entry out of range").map(|kept| (n, kept))) {
        Ok((n, kept)) => log(format!("gravity: grant feedback: loaded {kept} of {n} keys from {FILE}")), // alloc: Cold
        Err(why) => log(format!("gravity: grant feedback: {FILE} ignored ({why}); its statements start from their estimates and the next checkpoint rewrites it")), // alloc: Cold
    }
    true
}

/// The file's entry count and body when it is whole and this process's.
fn parse<'b>(b: &'b [u8], id: &Ident) -> Result<(u32, &'b [u8]), &'static str> {
    if b.len() < HEADER || b[..8] != MAGIC {
        return Err("not a feedback file");
    }
    let w32 = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if w32(8) != VERSION {
        return Err("another format version");
    }
    let (count, len, crc) = (w32(HEADER - 12), w32(HEADER - 8) as usize, w32(HEADER - 4));
    if b.len() != HEADER + len || len != count as usize * feedback::ENTRY_BYTES {
        return Err("torn");
    }
    if crc32c::fin_crc32c(crc32c::pg_comp_crc32c(crc32c::pg_comp_crc32c(crc32c::CRC32C_INIT, &b[..HEADER - 4]), &b[HEADER..])) != crc {
        return Err("bad CRC");
    }
    if b[12..44] != id.build || w32(44) != id.build_len {
        return Err("another binary wrote it");
    }
    if b[48..56] != id.system.to_le_bytes() {
        return Err("another cluster wrote it");
    }
    Ok((count, &b[HEADER..]))
}

/// The checkpointer's call at the end of each checkpoint and restartpoint and at the shutdown checkpoint: every database's
/// history with a sample in the window, written whole to a temp file, fsynced and renamed over the file (durable_rename).
/// Nothing when feedback never ran since the start: an existing file stays as it is. Failures are LOG lines, never the
/// checkpoint's.
pub fn flush() {
    if KEEPS.get() != Some(&true) {
        return;
    }
    let Some((path, id)) = path().and_then(|p| Some((p, ident()?))) else { return };
    let mut b = Vec::with_capacity(HEADER); // alloc: Cold
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&VERSION.to_le_bytes());
    b.extend_from_slice(&id.build);
    b.extend_from_slice(&id.build_len.to_le_bytes());
    b.extend_from_slice(&id.system.to_le_bytes());
    b.extend_from_slice(&[0u8; 12]);
    let count = feedback::export(&mut b, feedback::now());
    let len = (b.len() - HEADER) as u32;
    b[HEADER - 12..HEADER - 8].copy_from_slice(&count.to_le_bytes());
    b[HEADER - 8..HEADER - 4].copy_from_slice(&len.to_le_bytes());
    let crc = crc32c::fin_crc32c(crc32c::pg_comp_crc32c(crc32c::pg_comp_crc32c(crc32c::CRC32C_INIT, &b[..HEADER - 4]), &b[HEADER..]));
    b[HEADER - 4..HEADER].copy_from_slice(&crc.to_le_bytes());
    let tmp = format!("{path}.tmp"); // alloc: Cold
    if let Err(e) = fd::sync::write_whole_file(&tmp, &b, true) {
        log(format!("gravity: grant feedback: could not write {tmp}: errno {e}")); // alloc: Cold
        let _ = fd::sync::pg_unlink(&tmp);
        return;
    }
    if !matches!(fd::sync::durable_rename(&tmp, &path, types_error::LOG), Ok(0)) {
        let _ = fd::sync::pg_unlink(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file is taken only whole and from this binary and cluster; each other one names why it is ignored.
    #[test]
    fn a_file_is_taken_only_whole_and_from_this_binary_and_cluster() {
        let id = Ident { build: [7; 32], build_len: 20, system: 0x7a3c_5e11_0042_9d01 };
        let body = vec![0u8; feedback::ENTRY_BYTES]; // alloc: Cold
        let file = |id: &Ident, body: &[u8], count: u32| {
            let mut b = Vec::new(); // alloc: Cold
            b.extend_from_slice(&MAGIC);
            b.extend_from_slice(&VERSION.to_le_bytes());
            b.extend_from_slice(&id.build);
            b.extend_from_slice(&id.build_len.to_le_bytes());
            b.extend_from_slice(&id.system.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(body);
            let crc = crc32c::fin_crc32c(crc32c::pg_comp_crc32c(crc32c::pg_comp_crc32c(crc32c::CRC32C_INIT, &b[..HEADER - 4]), &b[HEADER..]));
            b[HEADER - 4..HEADER].copy_from_slice(&crc.to_le_bytes());
            b
        };
        let good = file(&id, &body, 1);
        assert_eq!(parse(&good, &id).map(|(n, b)| (n, b.len())), Ok((1, feedback::ENTRY_BYTES)));
        let mut flipped = good.clone();
        flipped[HEADER + 3] ^= 1;
        assert_eq!(parse(&flipped, &id).err(), Some("bad CRC"));
        assert_eq!(parse(&good[..good.len() - 7], &id).err(), Some("torn"));
        assert_eq!(parse(&file(&id, &body, 2), &id).err(), Some("torn"), "a count past the body");
        assert_eq!(parse(&file(&Ident { build: [8; 32], ..id }, &body, 1), &id).err(), Some("another binary wrote it"));
        assert_eq!(parse(&file(&Ident { system: id.system + 1, ..id }, &body, 1), &id).err(), Some("another cluster wrote it"));
        assert_eq!(parse(b"PGRGFBK", &id).err(), Some("not a feedback file"));
        // version 1's header (the cluster named by a 16-byte fingerprint seed): ignored, and the next flush rewrites it
        let mut v1 = Vec::new(); // alloc: Cold
        v1.extend_from_slice(&MAGIC);
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&id.build);
        v1.extend_from_slice(&id.build_len.to_le_bytes());
        v1.extend_from_slice(&[9; 16]);
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v1.extend_from_slice(&[0; 4]);
        v1.extend_from_slice(&body);
        assert_eq!(parse(&v1, &id).err(), Some("another format version"));
    }
}
