//! pgtz.c: tz directory, pg_tzset cache, session/log timezone globals.
//!
//! The cache is PER SESSION like C's `timezone_cache` (pgtz.c:209, freed
//! with the backend): a thread-local map of shared handles (`PgTzRef`),
//! released at session exit via mcx session cleanup, no cap on POSIX zones.
//! Handles, not `&'static` pointers: a zone escapes its session (the
//! process-shared DynamicZoneAbbrev table, GUC extras, log_timezone on
//! helper threads), and each holder's own reference keeps it alive -- the
//! earlier session-arena attempt's use-after-free (localtime/clock.rs:32).

use core::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use localtime::{pg_tz_acceptable, tzload, tzparse, PgTz, PgTzRef, TzLoadError, TzState, TZ_STRLEN_MAX};
use pgstrcasecmp::pg_toupper;
use types_core::primitive::MAXPGPATH;
use types_error::{PgError, PgResult, ERROR, LOG};

#[cfg(test)]
mod tests;

const SECS_PER_HOUR: i64 = 3600;
const SECS_PER_MINUTE: i64 = 60;

// The session/log timezone globals live in `localtime::globals` (below elog,
// which formats log timestamps in log_timezone); re-exported here so callers
// keep pgtz.c's names.
pub use localtime::globals::{
    log_timezone, session_timezone, set_log_timezone, set_session_timezone, with_session_timezone,
};
use elog::PCT_M;

// DIVERGENCE from pg_TZDIR: PGRUST_TZDIR (runtime) / PGRUST_PGSHAREDIR
// (build) take precedence over C's get_share_path resolution (harness
// override; boot-smoke.sh exports them), and a share/timezone staged beside
// the binary (main_main build.rs) is found without an install step.
/// `<exe dir>/share/<sub>` when the build staged it (main_main build.rs).
fn staged_share_dir(sub: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("share").join(sub);
    dir.is_dir().then(|| dir.to_string_lossy().into_owned())
}

/// C's `SYSTEMTZDIR` (configure --with-system-tzdata=DIR), here the
/// build-time `PGRUST_SYSTEM_TZDATA`.
const SYSTEM_TZDIR: Option<&str> = option_env!("PGRUST_SYSTEM_TZDATA");

fn pg_tzdir() -> &'static str {
    static TZDIR: OnceLock<String> = OnceLock::new();
    TZDIR.get_or_init(|| {
        if let Ok(dir) = std::env::var("PGRUST_TZDIR") {
            return dir;
        }
        resolve_tzdir(SYSTEM_TZDIR, staged_share_dir("timezone"))
    })
}

fn resolve_tzdir(system_tzdir: Option<&str>, staged: Option<String>) -> String {
    if let Some(sys) = system_tzdir {
        return sys.to_string();
    }
    if let Some(staged) = staged {
        return staged;
    }
    let mut dir = if let Some(share) = option_env!("PGRUST_PGSHAREDIR") {
        String::from(share)
    } else {
        let exec = init_small::globals::my_exec_path();
        let len = exec.iter().position(|&b| b == 0).unwrap_or(exec.len());
        pg_path::get_share_path(&String::from_utf8_lossy(&exec[..len]))
    };
    dir.push_str("/timezone");
    dir.truncate(MAXPGPATH - 1);
    dir
}

fn read_file_into(path: &str, buf: &mut [u8]) -> Option<usize> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut nread = 0usize;
    while nread < buf.len() {
        match file.read(&mut buf[nread..]) {
            Ok(0) => break,
            Ok(n) => nread += n,
            Err(_) => return None,
        }
    }
    Some(nread)
}

/// pg_open_tzfile + tzload's single read; `Ok(None)` is C's -1.
pub fn pg_open_tzfile(
    name: &[u8],
    canonname: Option<&mut [u8; TZ_STRLEN_MAX + 1]>,
    buf: &mut [u8],
) -> PgResult<Option<usize>> {
    let tzdir = pg_tzdir();
    let orignamelen = tzdir.len();

    if orignamelen + 1 + name.len() >= MAXPGPATH {
        return Ok(None); /* not gonna fit */
    }

    // Zone names are ASCII; non-UTF-8 cannot match an openable file.
    let Ok(name_str) = core::str::from_utf8(name) else {
        return Ok(None);
    };

    if canonname.is_none() {
        let mut asis = String::with_capacity(orignamelen + 1 + name.len());
        asis.push_str(tzdir);
        asis.push('/');
        asis.push_str(name_str);
        if let Some(n) = read_file_into(&asis, buf) {
            return Ok(Some(n));
        }
    }

    let mut fullname = String::with_capacity(MAXPGPATH);
    fullname.push_str(tzdir);
    let mut fname = name_str;
    loop {
        let (level, rest) = match fname.find('/') {
            Some(slash) => (&fname[..slash], Some(&fname[slash + 1..])),
            None => (fname, None),
        };
        let Some(canon) = scan_directory_ci(&fullname, level.as_bytes())? else {
            return Ok(None);
        };
        fullname.push('/');
        fullname.push_str(&canon);
        match rest {
            Some(r) => fname = r,
            None => break,
        }
    }

    if let Some(canonname) = canonname {
        let canonical = fullname[orignamelen + 1..].as_bytes();
        let n = canonical.len().min(TZ_STRLEN_MAX);
        canonname[..n].copy_from_slice(&canonical[..n]);
        canonname[n] = 0;
    }

    Ok(read_file_into(&fullname, buf))
}

// Hidden entries are skipped (security: no escape from the tz directory);
// read failures are LOG severity, as in C.
fn scan_directory_ci(dirname: &str, fname: &[u8]) -> PgResult<Option<String>> {
    let dir = fd::AllocateDir(dirname)?;
    let mut found = None;
    while let Some(entry) = fd::ReadDirExtended(dir, dirname, LOG)? {
        if entry.d_name.as_bytes().first() == Some(&b'.') {
            continue;
        }
        let ebytes = entry.d_name.as_bytes();
        if ebytes.len() == fname.len() && ebytes.eq_ignore_ascii_case(fname) {
            found = Some(entry.d_name);
            break;
        }
    }
    fd::FreeDir(dir)?;
    Ok(found)
}

thread_local! {
    // pgtz.c timezone_cache; `None` until the first resolve arms the release.
    static TIMEZONE_CACHE: RefCell<Option<BTreeMap<Box<[u8]>, PgTzRef>>> =
        const { RefCell::new(None) };
}

fn cache_lookup(uppername: &[u8]) -> Option<PgTzRef> {
    TIMEZONE_CACHE.with(|c| c.borrow().as_ref().and_then(|m| m.get(uppername).cloned()))
}

fn cache_insert(uppername: &[u8], tz: PgTz) -> PgTzRef {
    TIMEZONE_CACHE.with(|c| {
        let mut slot = c.borrow_mut();
        let map = slot.get_or_insert_with(|| {
            mcx::register_session_cleanup(Box::new(release_session_cache));
            BTreeMap::new()
        });
        map.entry(uppername.into()).or_insert_with(|| PgTzRef::new(tz)).clone()
    })
}

/// Session exit: drop this session's entries and its session timezone handle
/// (zones referenced elsewhere live on through their own handles).
pub fn release_session_cache() {
    TIMEZONE_CACHE.with(|c| *c.borrow_mut() = None);
    set_session_timezone(None);
}

/// Zones this session still caches (0 after `release_session_cache`).
pub fn session_cache_len() -> usize {
    TIMEZONE_CACHE.with(|c| c.borrow().as_ref().map_or(0, BTreeMap::len))
}

/// Load a timezone from file or cache; does not verify acceptability. "GMT"
/// always goes to tzparse(), never the filesystem, as in C. `Ok(None)` is
/// C's NULL (unknown zone); `Err` is an ereport that unwound out of tzload
/// (the AllocateDir descriptor-cap refusal, fd.c:2916) or the GMT elog.
pub fn pg_tzset(tzname: &[u8]) -> PgResult<Option<PgTzRef>> {
    if tzname.len() > TZ_STRLEN_MAX {
        return Ok(None); /* not going to fit */
    }

    let mut upper = [0u8; TZ_STRLEN_MAX + 1];
    for (dst, src) in upper.iter_mut().zip(tzname.iter()) {
        *dst = pg_toupper(*src);
    }
    let uppername = &upper[..tzname.len()];

    if let Some(tz) = cache_lookup(uppername) {
        return Ok(Some(tz));
    }

    let mut tzstate = Box::new(TzState::new());
    let mut canonname = [0u8; TZ_STRLEN_MAX + 1];

    if uppername == b"GMT" {
        if !tzparse(uppername, &mut tzstate, true) {
            /* This really, really should not happen ... */
            return Err(PgError::error("could not initialize GMT time zone").into());
        }
        canonname[..3].copy_from_slice(b"GMT");
    } else {
        match tzload(uppername, Some(&mut canonname), &mut tzstate, true) {
            Ok(()) => {}
            Err(TzLoadError::Report(e)) => return Err(e),
            Err(_) => {
                if uppername.first() == Some(&b':') || !tzparse(uppername, &mut tzstate, false) {
                    return Ok(None);
                }
                canonname[..uppername.len()].copy_from_slice(uppername);
            }
        }
    }

    Ok(Some(cache_insert(uppername, PgTz { tzname: canonname, state: *tzstate })))
}

/// Fixed-GMT-offset zone: seconds, positive = west of Greenwich (POSIX sign
/// convention); the displayable abbreviation uses the ISO convention.
pub fn pg_tzset_offset(gmtoffset: i64) -> PgResult<Option<PgTzRef>> {
    // pgtz.c:322 `absoffset = (gmtoffset < 0) ? -gmtoffset : gmtoffset`:
    // for LONG_MIN (check_timezone's (long) cast of an infinite/huge hours
    // value) C's negation wraps and the resulting "<+-...>" name fails
    // pg_tzset; the outcome is NULL, so return it directly.
    let mut absoffset = if gmtoffset < 0 {
        match gmtoffset.checked_neg() {
            Some(neg) => neg,
            None => return Ok(None),
        }
    } else {
        gmtoffset
    };

    let mut offsetstr = [0u8; 64];
    let mut olen = 0usize;
    push_2d(&mut offsetstr, &mut olen, absoffset / SECS_PER_HOUR);
    absoffset %= SECS_PER_HOUR;
    if absoffset != 0 {
        offsetstr[olen] = b':';
        olen += 1;
        push_2d(&mut offsetstr, &mut olen, absoffset / SECS_PER_MINUTE);
        absoffset %= SECS_PER_MINUTE;
        if absoffset != 0 {
            offsetstr[olen] = b':';
            olen += 1;
            push_2d(&mut offsetstr, &mut olen, absoffset);
        }
    }

    let mut tzname = [0u8; 136];
    let mut tlen = 0usize;
    let (open, join) = if gmtoffset > 0 {
        (b"<-".as_slice(), b">+".as_slice())
    } else {
        (b"<+".as_slice(), b">-".as_slice())
    };
    for part in [open, &offsetstr[..olen], join, &offsetstr[..olen]] {
        tzname[tlen..tlen + part.len()].copy_from_slice(part);
        tlen += part.len();
    }

    pg_tzset(&tzname[..tlen])
}

// snprintf "%02ld": two-digit zero pad, longer values unpadded.
fn push_2d(buf: &mut [u8], len: &mut usize, v: i64) {
    if v >= 100 {
        let mut digits = [0u8; 20];
        let mut n = 0usize;
        let mut v = v;
        while v > 0 {
            digits[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
        }
        for d in (0..n).rev() {
            buf[*len] = digits[d];
            *len += 1;
        }
    } else {
        buf[*len] = b'0' + (v / 10) as u8;
        buf[*len + 1] = b'0' + (v % 10) as u8;
        *len += 2;
    }
}

/// Called before GUC init so log_timezone is valid for elog timestamps.
pub fn pg_timezone_initialize() -> PgResult<()> {
    // pgtz.c:209 hash_create("Timezones"): the backend's context row; the
    // entries themselves live in the process-global cache above.
    let _: &'static mcx::MemoryContext = mcx::session_root("Timezones");
    let tz = pg_tzset(b"GMT")?;
    set_session_timezone(tz.clone());
    set_log_timezone(tz);
    Ok(())
}

const MAX_TZDIR_DEPTH: usize = 10;

/// C pg_tzenum: one open directory handle per depth.
pub struct PgTzEnum {
    baselen: usize,
    depth: isize,
    dirdesc: [Option<types_storage::Dir>; MAX_TZDIR_DEPTH],
    dirname: [Option<fd::PgPath>; MAX_TZDIR_DEPTH],
    tz: Box<PgTz>,
}

pub fn pg_tzenumerate_start() -> PgResult<PgTzEnum> {
    let startdir = fd::PgPath::from(pg_tzdir());
    let baselen = startdir.len() + 1;
    let dirdesc = fd::AllocateDirPath(&startdir)?;
    if dirdesc.is_none() {
        return Err(elog::ereport(ERROR)
            .errcode_for_file_access()
            .errmsg(format!("could not open directory \"{startdir}\": {PCT_M}"))
            .into_error()
            .into());
    }
    let mut e = PgTzEnum {
        baselen,
        depth: 0,
        dirdesc: [None; MAX_TZDIR_DEPTH],
        dirname: [const { None }; MAX_TZDIR_DEPTH],
        tz: Box::new(PgTz::new(&[], TzState::new())),
    };
    e.dirdesc[0] = dirdesc;
    e.dirname[0] = Some(startdir);
    Ok(e)
}

pub fn pg_tzenumerate_end(dir: PgTzEnum) -> PgResult<()> {
    let mut dir = dir;
    while dir.depth >= 0 {
        fd::FreeDir(dir.dirdesc[dir.depth as usize].take())?;
        dir.dirname[dir.depth as usize] = None;
        dir.depth -= 1;
    }
    Ok(())
}

pub fn pg_tzenumerate_next(dir: &mut PgTzEnum) -> PgResult<Option<&PgTz>> {
    while dir.depth >= 0 {
        let d = dir.depth as usize;
        let dirname = dir.dirname[d].clone().unwrap();

        let Some(entry) = fd::ReadDirEntry(dir.dirdesc[d], &dirname, ERROR)? else {
            fd::FreeDir(dir.dirdesc[d].take())?;
            dir.dirname[d] = None;
            dir.depth -= 1;
            continue;
        };

        if entry.d_name.first() == Some(&b'.') {
            continue;
        }

        let fullname = dirname.join(&entry.d_name);

        // pgtz.c:450 get_dirent_type(fullname, direntry, true, ERROR).
        if fd::get_dirent_type_path(&fullname, entry.d_type, true, ERROR)? == fd::PgFileType::Dir {
            if dir.depth >= (MAX_TZDIR_DEPTH - 1) as isize {
                return Err(
                    PgError::error("timezone directory stack overflow".to_string()).into(),
                );
            }
            let sub = fd::AllocateDirPath(&fullname)?;
            if sub.is_none() {
                return Err(elog::ereport(ERROR)
                    .errcode_for_file_access()
                    .errmsg(format!("could not open directory \"{fullname}\": {PCT_M}"))
                    .into_error()
                    .into());
            }
            dir.depth += 1;
            dir.dirdesc[dir.depth as usize] = sub;
            dir.dirname[dir.depth as usize] = Some(fullname);
            continue;
        }

        // tzload() not pg_tzset(), so the cache isn't filled.
        let relname = &fullname[dir.baselen..];
        match tzload(relname, None, &mut dir.tz.state, true) {
            Ok(()) => {}
            Err(TzLoadError::Report(e)) => return Err(e),
            Err(_) => continue, /* zone could not be loaded, ignore it */
        }

        let n = relname.len().min(TZ_STRLEN_MAX);
        dir.tz.tzname[..n].copy_from_slice(&relname[..n]);
        dir.tz.tzname[n] = 0;

        if !pg_tz_acceptable(&dir.tz) {
            continue; /* ignore leap-second zones */
        }

        return Ok(Some(&dir.tz));
    }

    Ok(None)
}

pub fn init_seams() {
    pgtz_seams::pg_open_tzfile::set(pg_open_tzfile);
}
