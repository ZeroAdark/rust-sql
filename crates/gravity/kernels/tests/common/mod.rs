// gravity_kernels_rig's root: a native integration-test rig that reads the program ABI's words as raw pointers. The
// site's wasm gate checks every workspace member for wasm32, where the rig is empty.
#![cfg(not(target_family = "wasm"))]
#![allow(dead_code)]

use gravity_abi::ctx::{ctx_size, hdr, lane, site_ptr, Answer, Out, ANSWER_MINMAX, HEADER_SIZE};
use gravity_abi::program::{ProgramBlock, SPILL_PARTS};
use gravity_abi::site::site;
use gravity_abi::status::{Status, ERROR, OK, STOP, WRITE_DONE};
use gravity_abi::{Ctx, Datum, Lane, ScanPos, Site};
use gravity_program::Program;

pub type Row = (Vec<Datum>, Vec<u8>);
pub type RowsFor = fn(&[Datum], &[u8]) -> Vec<Row>;

/// The kernel outcomes as the tests name them (the status vocabulary decoded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ret {
    Done,
    Suspended { out_full: bool },
    Recheck { site: u8 },
    Error,
    /// `status::raised(mask)`: a bit per abi `status::raise` code.
    Raised { mask: u64 },
    Aborted,
    Unsupported { kind: u8 },
    Fault { site: u8, lane: u8, code: u32, part: u32 },
    Bad(i64),
}

impl Ret {
    pub fn from_i64(v: i64) -> Ret {
        match Status::decode(v) {
            Status::Ok(0) => Ret::Done,
            Status::Suspend { out_full } => Ret::Suspended { out_full },
            Status::Recheck { site } => Ret::Recheck { site },
            Status::Error => Ret::Error,
            Status::Raised { mask } => Ret::Raised { mask },
            Status::Cancelled => Ret::Aborted,
            Status::Unsupported { kind } => Ret::Unsupported { kind },
            Status::Fault { site, lane, code, part } => Ret::Fault { site, lane, code, part },
            Status::Ok(x) => Ret::Bad(x),
            Status::Stop | Status::BuildWait { .. } => Ret::Bad(v),
            Status::Bad(x) => Ret::Bad(x),
        }
    }
}

pub struct FakeSite {
    pub index: usize,
    pub rows_for: RowsFor,
    pub queue: Vec<Row>,
    pub positioned: Vec<Vec<Datum>>,
    pub fail_next_after: Option<usize>,
    pub fail_seek: bool,
    pub nexts: usize,
    pub served: u32,
    pub natts: usize,
    pub nkeys: usize,
    pub rel: u32,
    /// Rows per window (`next` transposes that many queued rows into column lanes).
    pub granule: usize,
    pub windows: usize,
    pub lanes: Vec<Lane>,
    /// Every window from this one on (1-based) is published as `AllMatch` with lane 0 undecoded (the zone verdict).
    pub all_match_from: Option<usize>,
    /// Every window from this one on (1-based) is published answered: `rows`, no lanes.
    pub answered_from: Option<usize>,
    /// The page protocol (`SiteDesc::raw_pages`): the window's rows also formed as heap tuples on one 8 KB page — `(attno, typlen,
    /// lane)` for every attribute 1..=max of the relation (by-value widths; the site's lane index holding it, −1 = not read: zero); the
    /// page, its flags and the offset lane are published beside the lanes.
    pub page_atts: Option<Vec<(i16, u8, i16)>>,
    /// The page protocol's clean form: no short tuple, and an unread varlena only in its 1-byte-header form (never NULL), so every
    /// window of NULL-free rows is a `PAGE_FULL | PAGE_NONULLS` page (the plain clone) whatever its length, 80 tuples within 8 KB.
    pub page_clean: bool,
    pub page_buf: Vec<u64>,
    pub page_offs: Vec<u16>,
    /// Lanes published as dictionary code lanes (`KIND_CODES`, 64 codes, width 8 elements, `aux` = code 0 the empty string).
    pub codes_lanes: u32,
    /// Lanes published as fixed mantissa lanes (`KIND_FIXED`, width 8, scale 2: the tpch kernels' money columns).
    pub fixed_lanes: u32,
    /// Bit `g`: global granule `g` (the seek's first key + the windows since it) publishes its `fixed_lanes` as datum lanes (a verbatim chunk).
    pub verbatim_granules: u64,
    /// Windows per part: window `w` since the seek (1-based) is granule `(w - 1) % wpp` of part
    /// `keys[0] / wpp + (w - 1) / wpp`, its position the global granule `keys[0] + w - 1` (0: one part, granule = window).
    pub windows_per_part: usize,
    /// Code lanes published as their part's dictionary entries (`FLAG_ENTRIES`) in the parts below `entries_parts`.
    pub entries_lanes: u32,
    pub entries_parts: u32,
    /// An entries part publishes each code once (the source's entries window: one row per code), not its rows flagged.
    pub entries_dedupe: bool,
    /// The render checks its plan as the source does (a code lane of the site, a part of the scan): the lowerer's
    /// rigs set it; the kernel tests render lanes / parts they never published.
    pub dict_plan: bool,
    seen_part: u32,
    seen: u64,
    seek_key0: u64,
    seek_windows: usize,
    /// The code lanes' dictionary (entry bytes per code) and the site's table lanes (`LaneDesc::table(i)` after
    /// the offset lane): `lane_fill` builds table `i` over `dict` with `gravity_dict` and publishes it.
    pub dict: Vec<Vec<u8>>,
    pub tables: Vec<RigTable>,
    /// The images `h_dict` rendered (the emit's winners point into them).
    pub rendered: Vec<Vec<u8>>,
    pub table_bufs: Vec<Vec<u64>>,
    /// The program tables swept and published (`LANE_DONE`), per (table, part): a later bind of the part finds the filled table.
    pub program_done: Vec<(usize, u32, usize)>,
    /// D3115: the next code each unfilled program table's sweep hands out (`LANE_CLAIM`), per (table, part).
    pub program_claims: Vec<(usize, u32, u64)>,
    /// An answered window's records, one per lane, computed from the transposed rows.
    pub answers: Vec<Answer>,
    /// The published width of each by-value lane (8; 2 / 4 publish the cells narrowed, as the granule source does).
    pub widths: Vec<u8>,
    narrow: Vec<Vec<u8>>,
    cols: Vec<Vec<Datum>>,
    valid: Vec<u64>,
}

pub const MAX_GRANULE: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    pub site: usize,
    pub op: u32,
    pub pos: ScanPos,
    pub values: Vec<Datum>,
    pub isnull: Vec<u8>,
}

pub struct FakeHost {
    pub ctx: *mut Ctx,
    pub sites: Vec<FakeSite>,
    pub calls: Vec<String>,
    pub pushed: Vec<Vec<Datum>>,
    pub pushed_nulls: Vec<Vec<u8>>,
    /// Each push's by-reference region, kept (lower.md §15.13): a pushed datum inside the region points into its copy.
    pub pushed_regions: Vec<Vec<u64>>,
    pub stop_after_rows: Option<usize>,
    pub out_natts: usize,
    pub writes: Vec<Written>,
    pub write_result: i64,
    pub lock_results: Vec<i64>,
    pub locks: Vec<ScanPos>,
    pub chunks: Vec<Box<[u8]>>,
    /// The sort runs (`SPILL_RUN_PUT` appends the region's `SortRun`; a test may load them) and each run's read offset.
    pub runs: Vec<Vec<u8>>,
    pub run_read: Vec<usize>,
    /// V4R-136: each put run's rows (`SPILL_RUN_ROWS` sums them).
    pub run_rows: Vec<u64>,
    /// The section ends the rig issued (one per finishing pipeline that ended `Done`).
    pub section_ends: usize,
    /// Where `SPILL_RUN_FILL` lands (`SPILL_RUN_BASE`); null: `Partials.own`. Null at every pipeline start.
    pub fill_base: *mut u8,
    /// `SPILL_RUN_OPEN` / `SPILL_RUN_FILL` calls served (a reader's host calls are per buffer, never per row).
    pub run_ops: (usize, usize),
    /// D3259: the leader's seal answer `SPILL_SEAL_OVER` serves (true: the section's runs merged pass a region).
    pub seal_over: bool,
}

/// The rig never spills partitions (`SPILL_PARTS` says 0); the run ops serve `host.runs` in memory.
unsafe extern "C" fn h_spill(ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    use gravity_abi::program::{SPILL_RUNS, SPILL_RUN_ADOPT, SPILL_RUN_BASE, SPILL_RUN_CLOSE, SPILL_RUN_FILL, SPILL_RUN_OPEN, SPILL_RUN_PUT};
    let h = host_of(ctx);
    let own = gravity_abi::program::partial(ctx);
    match op {
        SPILL_PARTS => 0,
        SPILL_RUN_PUT => {
            let run = &mut *(own as *mut gravity_abi::breaker::SortRun);
            // a var run's put compacted it into `var_bytes` of records (M4 L3B), else `len × row_bytes`
            h.runs.push(core::slice::from_raw_parts(run.row(0), run.dense_bytes()).to_vec());
            h.run_read.push(0);
            h.run_rows.push(run.len as u64);
            run.clear();
            0
        }
        SPILL_RUNS => h.runs.len() as i64,
        gravity_abi::program::SPILL_SEAL_OVER => h.seal_over as i64,
        gravity_abi::program::SPILL_RUN_ROWS => h.run_rows.iter().sum::<u64>() as i64,
        SPILL_RUN_OPEN => {
            // one run stream: the stream word (`(slot + 1) << 32`) is not part of the handle
            let r = (arg & 0xffff_ffff) as usize;
            h.run_read[r] = 0;
            h.run_ops.0 += 1;
            r as i64
        }
        SPILL_RUN_BASE => {
            h.fill_base = arg as usize as *mut u8;
            0
        }
        SPILL_RUN_CLOSE => 0,
        // NEW-43: the one run stream's runs dropped (`SortStreamRearm`)
        gravity_abi::program::SPILL_RUN_DROP => {
            (h.runs.clear(), h.run_read.clear(), h.run_rows.clear());
            0
        }
        // one run stream: an adoption moves nothing
        SPILL_RUN_ADOPT => 0,
        // D4373: a sort's work blocks (`SPILL_WORK_TAKE`, `SPILL_WORK_WITHIN`, the permutation's copy `SPILL_WORK_TRY`) take
        // the allocator's road, as under the memory policy's floor (the rig holds no grant)
        gravity_abi::program::SPILL_WORK_TAKE | gravity_abi::program::SPILL_WORK_WITHIN | gravity_abi::program::SPILL_WORK_TRY => 1,
        gravity_abi::program::SPILL_WORK_GIVE => 0,
        SPILL_RUN_FILL => {
            let (r, off, cap) = ((arg & 0xffff) as usize, ((arg >> 16) & 0xffff_ffff) as usize, (arg >> 48) as usize);
            let n = cap.min(h.runs[r].len() - h.run_read[r]);
            let base = if h.fill_base.is_null() { own } else { h.fill_base };
            h.run_ops.1 += 1;
            core::ptr::copy_nonoverlapping(h.runs[r].as_ptr().add(h.run_read[r]), base.add(off), n);
            h.run_read[r] += n;
            n as i64
        }
        _ => ERROR,
    }
}

/// `Program.arena_grow` as the driver's: a zeroed chunk of at least `bytes` (64 KiB minimum),
/// kept by the host for the rig's life, the bump window moved onto it.
unsafe extern "C" fn h_grow(ctx: *mut Ctx, bytes: u64) -> i64 {
    let h = host_of(ctx);
    let want = (bytes as usize).max(1 << 16);
    h.chunks.push(vec![0u8; want].into_boxed_slice());
    let c = h.chunks.last_mut().expect("chunk");
    (*ctx).arena_cur = c.as_mut_ptr();
    (*ctx).arena_end = c.as_mut_ptr().add(c.len());
    c.len() as i64
}

/// `Program.pin` as the driver's: a zeroed chunk kept for the rig's life, the bump window left alone.
unsafe extern "C" fn h_pin(ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    let h = host_of(ctx);
    h.chunks.push(vec![0u8; (bytes as usize).max(1 << 16)].into_boxed_slice());
    h.chunks.last_mut().expect("chunk").as_mut_ptr()
}

unsafe fn host<'a>(env: *mut u8) -> &'a mut FakeHost {
    &mut *(env as *mut FakeHost)
}

unsafe fn host_of<'a>(ctx: *mut Ctx) -> &'a mut FakeHost {
    host((*(*ctx).program).env)
}

/// `Site.next`: the seek from the key cells when the site is not open, then one window of
/// `granule` queued rows transposed into lanes (an answered or `AllMatch` window when asked).
unsafe extern "C" fn h_next(sp: *mut Site) -> i64 {
    let s = &mut *sp;
    let h = host(s.env);
    let site = &mut *(s.src as *mut FakeSite);
    let i = site.index;
    if s.state != site::STATE_OPEN {
        let keys = core::slice::from_raw_parts(s.keys, site.nkeys).to_vec();
        let nulls = core::slice::from_raw_parts(s.keys_null, site.nkeys).to_vec();
        h.calls.push(format!("seek({i}, {keys:?})"));
        if site.fail_seek {
            return ERROR;
        }
        let rows = (site.rows_for)(&keys, &nulls);
        site.seek_key0 = keys.first().copied().unwrap_or(0);
        site.seek_windows = site.windows;
        site.positioned.push(keys);
        site.queue = rows.into_iter().rev().collect();
        s.state = site::STATE_OPEN;
    }
    h.calls.push(format!("window({i})"));
    site.windows += 1;
    if site.fail_next_after.is_some_and(|n| site.windows > n) {
        return ERROR;
    }
    let mut take = site.queue.len().min(site.granule.clamp(1, MAX_GRANULE));
    // the entries face (lower.md §15.18): an entries part publishes each of its codes ONCE — a window's rows whose
    // code the part already published are dropped, a window left empty is passed over
    let ek = site.entries_lanes.trailing_zeros() as usize;
    let mut fill = None;
    while site.entries_dedupe && site.entries_lanes != 0 && site.windows_per_part > 0 && take > 0 {
        let part = (site.seek_key0 as usize / site.windows_per_part + (site.windows - site.seek_windows - 1) / site.windows_per_part) as u32;
        if part >= site.entries_parts {
            break;
        }
        if site.seen_part != part {
            (site.seen_part, site.seen) = (part, 0);
        }
        site.valid.fill(0);
        let mut r = 0;
        for _ in 0..take {
            let (v, n) = site.queue.pop().expect("queued");
            if site.seen >> v[ek] & 1 != 0 {
                continue;
            }
            site.seen |= 1 << v[ek];
            for k in 0..site.natts {
                site.cols[k][r] = v[k];
                site.valid[k] |= ((n[k] == 0) as u64) << r;
            }
            r += 1;
        }
        if r > 0 || site.queue.is_empty() {
            fill = Some(r);
            break;
        }
        site.windows += 1;
        take = site.queue.len().min(site.granule.clamp(1, MAX_GRANULE));
    }
    if let Some(r) = fill {
        take = r;
    }
    s.rows = take as u32;
    s.cursor = 0;
    s.sel = core::ptr::null();
    s.sel_n = 0;
    s.nlanes = site.lanes.len() as u16;
    s.answered = 0;
    s.answer = core::ptr::null();
    s.rows_in += take as u64;
    if take == 0 {
        return 0;
    }
    if fill.is_none() {
        site.valid.fill(0);
    }
    for r in 0..if fill.is_some() { 0 } else { take } {
        let (v, n) = site.queue.pop().expect("queued");
        for k in 0..site.natts {
            site.cols[k][r] = v[k];
            site.valid[k] |= ((n[k] == 0) as u64) << r;
        }
    }
    let all = if take == 64 { u64::MAX } else { (1u64 << take) - 1 };
    if site.answered_from.is_some_and(|w| site.windows >= w) {
        for k in 0..site.natts {
            let mut a = Answer { flags: ANSWER_MINMAX, min: i64::MAX, max: i64::MIN, ..Answer::NONE };
            let mut sum = 0i128;
            for r in 0..take {
                if site.valid[k] >> r & 1 == 0 {
                    continue;
                }
                let v = site.cols[k][r] as i64;
                a.nonnull += 1;
                a.min = a.min.min(v);
                a.max = a.max.max(v);
                sum += v as i128;
            }
            a.set_sum(sum);
            site.answers[k] = a;
        }
        s.answer = site.answers.as_ptr();
        s.nlanes = 0;
        s.answered = 1;
        site.served += take as u32;
        s.pos = ScanPos::new(site.rel, site.windows as u32, 0);
        s.granule = site.windows as u32;
        return take as i64;
    }
    let all_match = site.all_match_from.is_some_and(|w| site.windows >= w);
    let wpp = site.windows_per_part;
    let (w, part) = if wpp > 0 { let w = site.windows - site.seek_windows - 1; (w, (site.seek_key0 as usize / wpp + w / wpp) as u32) } else { (0, 0) };
    for k in 0..site.natts {
        let valid = if site.valid[k] == all { core::ptr::null() } else { &site.valid[k] as *const u64 };
        let width = site.widths[k];
        let mut l = if all_match && k == 0 {
            Lane::byval(core::ptr::null(), core::ptr::null(), width, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 0))
        } else if width == 8 {
            Lane::byval(site.cols[k].as_ptr() as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32))
        } else {
            for r in 0..take {
                let v = site.cols[k][r];
                let w = width as usize;
                site.narrow[k][r * w..(r + 1) * w].copy_from_slice(&v.to_le_bytes()[..w]);
            }
            Lane::byval(site.narrow[k].as_ptr(), valid, width, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32))
        };
        if all_match {
            l.flags |= lane::VERDICT_ALL_MATCH;
        }
        if site.codes_lanes >> k & 1 != 0 {
            l.kind = lane::KIND_CODES;
            l.region = lane::REGION_DICTIONARY;
            l.ncodes_or_scale = 64;
        }
        let verbatim = site.verbatim_granules != 0 && {
            let g = (site.seek_key0 as usize).wrapping_add(site.windows.wrapping_sub(site.seek_windows + 1));
            g < 64 && site.verbatim_granules >> g & 1 != 0
        };
        if site.fixed_lanes >> k & 1 != 0 && !verbatim {
            l.kind = lane::KIND_FIXED;
            l.ncodes_or_scale = 2;
        }
        if site.entries_lanes >> k & 1 != 0 && part < site.entries_parts {
            l.flags |= lane::FLAG_ENTRIES;
        }
        *s.lanes.add(k) = l;
    }
    if let Some(atts) = site.page_atts.clone() {
        publish_page(site, s, &atts, take);
    }
    site.served += take as u32;
    if wpp > 0 {
        s.part_id = part;
        s.granule = (w % wpp) as u32;
        s.pos = ScanPos::new(site.rel, site.seek_key0 as u32 + w as u32, 0);
        return take as i64;
    }
    s.pos = ScanPos::new(site.rel, site.windows as u32, 0);
    s.granule = site.windows as u32;
    take as i64
}

/// A rig table lane: the per-entry function over the site's `dict` (the source builds these in production).
#[derive(Clone, Debug)]
pub enum RigTable {
    Like { pattern: Vec<u8>, negated: bool },
    OctetLength,
    CharLength,
    Fingerprint,
    /// A derived fingerprint table: the fingerprints of these per-code values (`regexp_replace` of the entries, computed by the test).
    DerivedFingerprint(Vec<Vec<u8>>),
    /// A `table_fn::PROGRAM` table: allocated unfilled (`LANE_UNFILLED`), swept by the program, published at `LANE_DONE`.
    Program { elem: u8 },
}

struct RigDict<'a>(&'a [Vec<u8>]);

impl gravity_dict::Dict for RigDict<'_> {
    fn ncodes(&self) -> u32 {
        self.0.len() as u32
    }
    fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
        for (i, e) in self.0.iter().enumerate() {
            f(i as u32, gravity_dict::View::of(e));
        }
    }
}

unsafe extern "C" fn h_lane(sp: *mut Site, k: u32) -> i64 {
    let s = &mut *sp;
    let site = &mut *(s.src as *mut FakeSite);
    if k & gravity_abi::site::LANE_CLAIM != 0 {
        let i = (k & !gravity_abi::site::LANE_CLAIM) as usize - (site.natts + 1);
        let n = site.dict.len() as u64;
        let part = s.part_id;
        let at = match site.program_claims.iter().position(|(j, p, _)| *j == i && *p == part) {
            Some(at) => at,
            None => {
                site.program_claims.push((i, part, 0));
                site.program_claims.len() - 1
            }
        };
        let next = site.program_claims[at].2;
        site.program_claims[at].2 = (next + gravity_abi::site::SWEEP_CHUNK).min(n);
        return next.min(n) as i64;
    }
    if k & gravity_abi::site::LANE_DONE != 0 {
        let k = k & !gravity_abi::site::LANE_DONE;
        host(s.env).calls.push(format!("lane_done({}, {k})", site.index));
        let i = k as usize - (site.natts + 1);
        let hdr = (*s.lanes.add(k as usize)).ptr as usize;
        site.program_done.push((i, s.part_id, hdr));
        return 1;
    }
    host(s.env).calls.push(format!("lane({}, {k})", site.index));
    let Some(i) = (k as usize).checked_sub(site.natts + 1) else { return 0 };
    let Some(t) = site.tables.get(i).cloned() else { return 0 };
    if let Some((_, _, hdr)) = site.program_done.iter().find(|(j, part, _)| *j == i && *part == s.part_id) {
        gravity_dict::lane::publish(*hdr as *const gravity_dict::table::Header, i as u32, 0, &mut *s.lanes.add(k as usize));
        return 1;
    }
    if site.table_bufs.len() <= i {
        site.table_bufs.resize_with(i + 1, || vec![0u64; 1 << 14]);
    }
    let buf = &mut site.table_bufs[i];
    let mut region = gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8);
    let dict = RigDict(&site.dict);
    let hdr = match t {
        RigTable::Like { pattern, negated } => {
            let mut m = gravity_dict::like::Matcher::new(&pattern, gravity_dict::like::Mode::Like, &mut region).expect("pattern");
            if negated {
                m = m.negated();
            }
            m.build_bits(&dict, &mut region).expect("bits").header()
        }
        RigTable::OctetLength => gravity_dict::I64::build(&dict, &mut region, &mut |v| Ok(v.bytes().len() as i64)).expect("i64").header(),
        RigTable::CharLength => gravity_dict::I64::build(&dict, &mut region, &mut |v| Ok(v.bytes().iter().filter(|b| (**b & 0xC0) != 0x80).count() as i64)).expect("i64").header(),
        RigTable::Fingerprint => {
            gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
            gravity_dict::Fp128::build(&dict, &mut region, &mut |v| Ok(v)).expect("fp128").header()
        }
        RigTable::DerivedFingerprint(values) => {
            gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
            gravity_dict::Fp128::build(&RigDict(&values), &mut region, &mut |v| Ok(v)).expect("fp128").header()
        }
        RigTable::Program { elem } => {
            use gravity_dict::table::{Header, FLAG_FAULTS};
            gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
            let n = site.dict.len();
            let bytes = match elem {
                gravity_dict::lane::ELEM_BITS => n.div_ceil(64) * 8,
                gravity_dict::lane::ELEM_I64 => n * 8,
                _ => n * 16,
            };
            let hdr = region.alloc_zeroed(core::mem::size_of::<Header>(), 8).expect("header") as *mut Header;
            let data = region.alloc_zeroed(bytes.max(8), 8).expect("data");
            let faults = region.alloc_zeroed((n.div_ceil(64) * 8).max(8), 8).expect("faults") as *const u64;
            (*hdr).elem = elem;
            (*hdr).flags = FLAG_FAULTS;
            (*hdr).ncodes = n as u32;
            (*hdr).data = data;
            (*hdr).faults = faults;
            (*hdr).base = region.base();
            gravity_dict::lane::publish(hdr, i as u32, 0, &mut *s.lanes.add(k as usize));
            return gravity_abi::site::LANE_UNFILLED;
        }
    };
    let over = 0u8;
    gravity_dict::lane::publish(hdr, i as u32, over, &mut *s.lanes.add(k as usize));
    1
}

/// The rig's render: the site's `dict` entry as a 4B-U varlena image kept for the rig's life.
unsafe extern "C" fn h_dict(sp: *mut Site, part: u32, lane: u32, code: u32, out: *mut u64) -> i64 {
    let s = &mut *sp;
    let site = &mut *(s.src as *mut FakeSite);
    // the source's plan check (`granule_source.rs` `dict_value_pg`): a code lane of the site, a part of the scan
    let parts = if site.windows_per_part > 0 { site.windows.div_ceil(site.windows_per_part).max(1) } else { 1 };
    if site.dict_plan && (lane as usize >= site.natts || site.codes_lanes >> lane & 1 == 0 || part as usize >= parts) {
        host(s.env).calls.push(format!("dict({}, {part}, {lane}, {code}) outside the plan", site.index));
        return -1;
    }
    host(s.env).calls.push(if site.windows_per_part > 0 { format!("dict({}, {part}, {code})", site.index) } else { format!("dict({}, {code})", site.index) });
    let Some(e) = site.dict.get(code as usize) else { return -1 };
    let mut img = Vec::with_capacity(4 + e.len());
    img.extend_from_slice(&(((4 + e.len()) as u32) << 2).to_le_bytes());
    img.extend_from_slice(e);
    site.rendered.push(img);
    *out = site.rendered.last().unwrap().as_ptr() as u64;
    OK
}

/// `Site.fetch` as the fetch source serves it: the `n` locators at the site's rowid lane (`part << 32 | granule << 13 | row`
/// over this site's `rows_for` rows in window order: `windows_per_part` and `granule` as the walking site's) into the
/// attribute lanes (datum words, width 8, a validity word per lane; `n ≤ MAX_GRANULE`); `n`, or a status below zero.
unsafe extern "C" fn h_fetch(sp: *mut Site, n: u32) -> i64 {
    let s = &mut *sp;
    let site = &mut *(s.src as *mut FakeSite);
    host(s.env).calls.push(format!("fetch({}, {n})", site.index));
    let lanes = core::slice::from_raw_parts_mut(s.lanes, s.nlanes.max(1) as usize);
    let Some(req) = lanes.iter().find(|l| lane::id_kind(l.id) == lane::ID_ROWID && !l.ptr.is_null()) else { return ERROR };
    let n = n as usize;
    if n > MAX_GRANULE {
        return ERROR;
    }
    let ids = core::slice::from_raw_parts(req.ptr as *const u64, n);
    let rows = (site.rows_for)(&[], &[]);
    let g = site.granule.clamp(1, MAX_GRANULE);
    site.valid.fill(0);
    for (i, &id) in ids.iter().enumerate() {
        let (part, granule, row) = ((id >> 32) as usize, (id >> 13 & 0x7ffff) as usize, (id & 0x1fff) as usize);
        // one part: the rig numbers windows from 1 (`s.granule = windows`); parts: `part * wpp + granule` from 0
        let w = if site.windows_per_part > 0 { part * site.windows_per_part + granule } else { granule - 1 };
        let Some((v, nl)) = rows.get(w * g + row) else { return ERROR };
        for k in 0..site.natts {
            site.cols[k][i] = v[k];
            site.valid[k] |= ((nl[k] == 0) as u64) << i;
        }
    }
    let all = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
    for k in 0..site.natts {
        let valid = if site.valid[k] == all { core::ptr::null() } else { &site.valid[k] as *const u64 };
        lanes[k] = Lane::byval(site.cols[k].as_ptr() as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32));
    }
    n as i64
}

unsafe extern "C" fn h_write(ctx: *mut Ctx, op: u32, a: u64) -> i64 {
    let h = host_of(ctx);
    let i = a as usize;
    let s = &*site_ptr(h.ctx, i);
    let n = h.sites[i].natts;
    let w = Written { site: i, op, pos: s.pos, values: core::slice::from_raw_parts(s.values, n).to_vec(), isnull: core::slice::from_raw_parts(s.isnull, n).to_vec() };
    h.calls.push(format!("write({i}, {op})"));
    h.writes.push(w);
    h.write_result
}

unsafe extern "C" fn h_lock(ctx: *mut Ctx, a: u64) -> i64 {
    let h = host_of(ctx);
    let s = &*site_ptr(h.ctx, a as usize);
    h.calls.push(format!("lock({a})"));
    h.locks.push(s.pos);
    if h.lock_results.is_empty() {
        WRITE_DONE
    } else {
        h.lock_results.remove(0)
    }
}

unsafe extern "C" fn h_push(ctx: *mut Ctx) -> i64 {
    let h = host_of(ctx);
    let o = &mut *(*ctx).out;
    let rows = o.count as usize;
    let k = h.out_natts;
    let (lo, len) = (o.region as u64, o.region_len as u64);
    let kept = if len > 0 { core::slice::from_raw_parts(o.region as *const u64, len as usize / 8).to_vec() } else { Vec::new() };
    for r in 0..rows {
        let mut row = core::slice::from_raw_parts(o.values.add(r * k), k).to_vec();
        for d in row.iter_mut().filter(|d| len > 0 && **d >= lo && **d < lo + len) {
            *d = kept.as_ptr() as u64 + (*d - lo);
        }
        h.pushed.push(row);
        h.pushed_nulls.push(core::slice::from_raw_parts(o.isnull.add(r * k), k).to_vec());
    }
    h.calls.push(format!("push({rows})"));
    h.pushed_regions.push(kept);
    o.count = 0;
    o.region_len = 0;
    if h.stop_after_rows.is_some_and(|n| h.pushed.len() >= n) {
        STOP
    } else {
        OK
    }
}

/// The share a Rig arms a share-budgeted breaker region with.
pub const RIG_SHARE: u32 = 1 << 20;

pub struct Rig {
    pub program: Program,
    pub buf: Vec<u64>,
    pub host: Box<FakeHost>,
    pub out_values: Vec<Datum>,
    pub out_isnull: Vec<u8>,
    pub out_region: Vec<u64>,
    pub rows: Vec<Row>,
    pub keys: Vec<Row>,
    pub params: Vec<Datum>,
    pub param_nulls: Vec<u8>,
    pub scratch: Vec<u64>,
    pub out: Box<Out>,
    pub prog: Box<ProgramBlock>,
    pub partial: Vec<u64>,
    /// The leader region a consumer reads (`consumed[0]`): the producer's partial folded into it
    /// after its pipeline (a worker's build partition becomes the merged hashed table).
    pub leader: Vec<u64>,
    /// The further leader regions: one per distinct produced slot past the first (`slot_lead`: slot → leader index,
    /// 0 = `leader`), so a consumer reads every slot it consumes at its place among its `consumes` (`consumed[k]`,
    /// `more[k]`) — a join chain probes several builds, a chain build probes its own.
    pub extra_leaders: Vec<Vec<u64>>,
    slot_lead: Vec<(u8, usize)>,
    /// `Partials.more` of the open pipeline: its consumed slots' leader regions in `consumes` order.
    more: Vec<*mut u8>,
    /// The leader the current producer folds into (its slot's).
    lead_at: usize,
    partials: Box<gravity_abi::Partials>,
    pub interrupt: Box<u8>,
}

impl Rig {
    pub fn new(program: Program, params: Vec<Datum>) -> Rig {
        Rig::with_nulls(program, params.clone(), vec![0u8; params.len().max(1)])
    }

    pub fn with_nulls(program: Program, params: Vec<Datum>, param_nulls: Vec<u8>) -> Rig {
        let l = program.layout;
        let nsites = program.sites.len();
        let mut buf = vec![0u64; ctx_size(nsites) / 8];
        let ctx = buf.as_mut_ptr() as *mut Ctx;
        let out_n = l.out_rows as usize * l.out_natts as usize;
        let mut out_values = vec![0u64; out_n.max(1)];
        let mut out_isnull = vec![0u8; out_n.max(1)];
        let mut out_region = vec![0u64; (program.out_region as usize).div_ceil(8)];
        let mut scratch = vec![0u64; (l.scratch_bytes as usize).div_ceil(8).max(1)];
        // a share-budgeted breaker gets a `RIG_SHARE` region armed the way the driver arms it
        let shared = program.slots.shared.iter().any(|&s| s);
        let mut partial = vec![0u64; (program.worker_partial_bytes() as usize).max(if shared { RIG_SHARE as usize } else { 0 }).div_ceil(8).max(1)];
        let mut leader = vec![0u64; partial.len()];
        if shared {
            unsafe { gravity_abi::breaker::HashAgg::arm(partial.as_mut_ptr() as *mut u8, RIG_SHARE) };
            unsafe { gravity_abi::breaker::HashAgg::arm(leader.as_mut_ptr() as *mut u8, RIG_SHARE) };
        }
        // one leader region per distinct produced slot: index 0 (`leader`) = the slot the LAST pipeline reads (the
        // tests' `rig.leader`: rung 6's merged run, rung 9's agg table), the rest in producer order (two passes over
        // one slot share it)
        let mut slot_lead: Vec<(u8, usize)> = Vec::new();
        let last = program.pipelines.len() - 1;
        if let Some(s) = Self::slot_read_by(&program, last) {
            slot_lead.push((s, 0));
        }
        for s in program.pipelines.iter().filter_map(|p| p.produces()) {
            if !slot_lead.iter().any(|(t, _)| *t == s) {
                slot_lead.push((s, slot_lead.len()));
            }
        }
        let mut extra_leaders: Vec<Vec<u64>> = (1..slot_lead.len().max(1)).map(|_| vec![0u64; partial.len()]).collect();
        if shared {
            for l in extra_leaders.iter_mut() {
                unsafe { gravity_abi::breaker::HashAgg::arm(l.as_mut_ptr() as *mut u8, RIG_SHARE) };
            }
        }
        let more: Vec<*mut u8> = vec![leader.as_mut_ptr() as *mut u8];
        let mut partials = Box::new(gravity_abi::Partials::two(partial.as_mut_ptr() as *mut u8, [more[0], core::ptr::null_mut()]));
        partials.more = more.as_ptr();
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut sites = Vec::new();
        for (i, d) in program.sites.iter().enumerate() {
            rows.push((vec![0u64; d.natts().max(1)], vec![0u8; d.natts().max(1)]));
            keys.push((vec![0u64; d.nkeys().max(1)], vec![0u8; d.nkeys().max(1)]));
            sites.push(FakeSite {
                index: i,
                rows_for: |_, _| Vec::new(),
                queue: Vec::new(),
                positioned: Vec::new(),
                fail_next_after: None,
                fail_seek: false,
                nexts: 0,
                served: 0,
                natts: d.natts(),
                nkeys: d.nkeys(),
                rel: d.relation,
                granule: 4,
                all_match_from: None,
                answered_from: None,
                codes_lanes: 0,
                page_atts: None,
                page_clean: false,
                page_buf: vec![0u64; 1024],
                page_offs: vec![0u16; 512],
                fixed_lanes: 0,
                verbatim_granules: 0,
                windows_per_part: 0,
                entries_lanes: 0,
                entries_parts: u32::MAX,
                entries_dedupe: false,
                dict_plan: false,
                seen_part: u32::MAX,
                seen: 0,
                seek_key0: 0,
                seek_windows: 0,
                dict: Vec::new(),
                tables: Vec::new(),
                rendered: Vec::new(),
                table_bufs: Vec::new(),
                program_done: Vec::new(),
                program_claims: Vec::new(),
                answers: vec![Answer::NONE; d.natts().max(1)],
                widths: vec![8; d.natts().max(1)],
                narrow: (0..d.natts()).map(|_| vec![0u8; MAX_GRANULE * 8]).collect(),
                windows: 0,
                lanes: vec![Lane::NONE; d.nlanes().max(1)],
                cols: (0..d.natts()).map(|_| vec![0; MAX_GRANULE]).collect(),
                valid: vec![0; d.natts().max(1)],
            });
        }
        let mut host = Box::new(FakeHost {
            ctx,
            sites,
            calls: Vec::new(),
            pushed: Vec::new(),
            pushed_nulls: Vec::new(),
            pushed_regions: Vec::new(),
            stop_after_rows: None,
            runs: Vec::new(),
            run_rows: Vec::new(),
            run_read: Vec::new(),
            section_ends: 0,
            fill_base: core::ptr::null_mut(),
            run_ops: (0, 0),
            seal_over: false,
            out_natts: l.out_natts as usize,
            writes: Vec::new(),
            write_result: WRITE_DONE,
            lock_results: Vec::new(),
            locks: Vec::new(),
            chunks: Vec::new(),
        });
        let interrupt = Box::new(0u8);
        let mut out = Box::new(Out { values: out_values.as_mut_ptr(), isnull: out_isnull.as_mut_ptr(), count: 0, cap: l.out_rows as i64, region: out_region.as_mut_ptr() as *mut u8, region_cap: program.out_region as i64, region_len: 0 });
        let env = &mut *host as *mut FakeHost as *mut u8;
        let mut prog = Box::new(ProgramBlock { push: Some(h_push), spill: Some(h_spill), arena_grow: Some(h_grow), write: Some(h_write), lock: Some(h_lock), partial: &*partials as *const gravity_abi::Partials as *mut gravity_abi::Partials, env, sink: core::ptr::null_mut(), rec_open: Some(gravity_abi::breaker::rec_open), owner: program.owner.as_deref().map_or(core::ptr::null_mut(), |o| o as *const dyn core::any::Any as *const u8 as *mut u8), pin: Some(h_pin), instr: core::ptr::null_mut() });
        unsafe {
            let h = &mut *ctx;
            h.program = &mut *prog;
            h.sites = (ctx as *mut u8).add(HEADER_SIZE) as *mut Site;
            h.out = &mut *out;
            h.nsites = nsites as u16;
            h.params = params.as_ptr();
            h.params_null = param_nulls.as_ptr();
            h.cancel = &*interrupt;
            h.arena_cur = scratch.as_mut_ptr() as *mut u8;
            h.arena_end = (scratch.as_mut_ptr() as *mut u8).add(scratch.len() * 8);
            h.claim = gravity_abi::MorselRange::ALL;
            h.width = 1;
            for i in 0..nsites {
                let s = &mut *site_ptr(ctx, i);
                s.values = rows[i].0.as_mut_ptr();
                s.isnull = rows[i].1.as_mut_ptr();
                s.keys = keys[i].0.as_mut_ptr();
                s.keys_null = keys[i].1.as_mut_ptr();
                s.lanes = host.sites[i].lanes.as_mut_ptr();
                s.lanes_cap = host.sites[i].lanes.len() as u16; // D1340: as the driver's node bind
                s.next = Some(h_next);
                s.lane = Some(h_lane);
                s.fetch = Some(h_fetch);
                s.dict = Some(h_dict);
                s.src = &mut host.sites[i] as *mut FakeSite as *mut u8;
                s.env = env;
                s.nkeys = host.sites[i].nkeys as u16;
                s.natts = host.sites[i].natts as u16;
            }
        }
        Rig { program, buf, host, out_values, out_isnull, out_region, rows, keys, params, param_nulls, scratch, out, prog, partial, leader, extra_leaders, slot_lead, more, lead_at: 0, partials, interrupt }
    }

    /// Point `own` at the producer's partial (zeroed and re-armed per producer, as the driver does a
    /// worker region) or, for a pipeline producing nothing, at the leader region.
    fn aim(&mut self, p: usize) {
        let produces = self.program.pipelines[p].produces().is_some();
        if produces {
            self.lead_at = self.lead_index(self.program.pipelines[p].produces().unwrap());
            self.partial.fill(0);
            // the driver arms a slot's region by its own budget (`node.rs`: `r.shared`), never another slot's
            if self.program.pipelines[p].breaker().is_some_and(|b| b.budget == gravity_abi::breaker::Budget::Share) {
                unsafe { gravity_abi::breaker::HashAgg::arm(self.partial.as_mut_ptr() as *mut u8, RIG_SHARE) };
            }
        }
        self.host.fill_base = core::ptr::null_mut();
        self.partials.own = if produces { self.partial.as_mut_ptr() } else { self.leader.as_mut_ptr() } as *mut u8;
        // the pipeline's view of what it consumes: its slots' leader regions in `consumes` order (a pipeline that
        // declares nothing sees the nearest earlier producer's slot at `consumed[0]` — the emit behind a build or
        // a two-pass, the chain every program before §2 is; the driver's "nearest earlier producer" law)
        let mut more: Vec<*mut u8> = self.program.pipelines[p].consumes.iter().map(|&s| self.lead_index(s)).collect::<Vec<_>>().into_iter().map(|k| self.lead_ptr(k)).collect();
        if more.is_empty() {
            let k = Self::slot_read_by(&self.program, p).map_or(0, |s| self.lead_index(s));
            more.push(self.lead_ptr(k));
        }
        self.more = more;
        self.partials.consumed = [self.more[0], self.more.get(1).copied().unwrap_or(core::ptr::null_mut())];
        self.partials.more = self.more.as_ptr();
    }

    /// The slot pipeline `p` reads at `consumed[0]`: its first declared `consumes`, else the nearest earlier
    /// producer's slot (none before it: the program's last producer, so a lone producer's region is `leader`).
    fn slot_read_by(program: &Program, p: usize) -> Option<u8> {
        program.pipelines[p].consumes.first().copied()
            .or_else(|| (0..p).rev().find_map(|q| program.pipelines[q].produces()))
            .or_else(|| program.pipelines.iter().rev().find_map(|q| q.produces()))
    }

    /// The leader region pipeline `p` reads at `consumed[0]` (rung 8 lays the admitted set where the second pass
    /// reads it: the first pass's slot, as the runner does).
    pub fn consumed_ptr(&mut self, p: usize) -> *mut u8 {
        let k = Self::slot_read_by(&self.program, p).map_or(0, |s| self.lead_index(s));
        self.lead_ptr(k)
    }

    fn lead_index(&self, slot: u8) -> usize {
        self.slot_lead.iter().find(|(t, _)| *t == slot).map_or(0, |(_, k)| *k)
    }

    fn lead_ptr(&mut self, k: usize) -> *mut u8 {
        match k {
            0 => self.leader.as_mut_ptr() as *mut u8,
            k => self.extra_leaders[k - 1].as_mut_ptr() as *mut u8,
        }
    }

    /// The leader region the current producer folds into (`lead_at`): `leader`, or one of the extra ones.
    fn lead(&mut self) -> &mut Vec<u64> {
        match self.lead_at {
            0 => &mut self.leader,
            k => &mut self.extra_leaders[k - 1],
        }
    }

    /// The driver's merge, in miniature: the partial folded into the leader region after its
    /// pipeline — a build partition hashed into a `BUILD_TABLE`, a phase-1 table through the
    /// shape's owner functions as one owner (the width-1 arm), any other table copied whole.
    fn fold_partial(&mut self, owner: &dyn Fn(&Program, &[Datum]) -> Option<gravity_abi::breaker::OwnerFns>) {
        use gravity_abi::breaker::{table_kind, HashBuild, BUILD_PARTITION, BUILD_TABLE, TABLE_BUILD, TABLE_T1};
        let src = self.partial.as_ptr() as *const u8;
        if self.partial.iter().all(|&w| w == 0) || (self.partial[0] == 0 && self.partial[1] as u32 == RIG_SHARE && self.partial[2..].iter().all(|&w| w == 0)) {
            return;
        }
        if unsafe { table_kind(src) } == TABLE_T1 {
            let fns = owner(&self.program, &self.params).expect("a phase-1 table's kernel brings its owner functions");
            return self.fold_owned(&fns);
        }
        if unsafe { table_kind(src) } == TABLE_BUILD && unsafe { HashBuild::view(src) }.table == BUILD_PARTITION {
            use gravity_abi::breaker::MAX_BUILD_PARTS;
            use gravity_abi::buildrows::RowStore;
            let p = unsafe { HashBuild::view(src) };
            // a partial with a row store (M7 form A): the leader's carved alike before its table is laid out, the slices
            // appended, every entry's payload rebased by its partition — `HashBuildPartial::merge_into` in miniature
            let mut rebase = [0u32; MAX_BUILD_PARTS];
            let lead_ptr = self.lead().as_mut_ptr() as *mut u8;
            let stored = unsafe { RowStore::of(src) }.map(|ps| {
                let ls = unsafe { RowStore::carve(lead_ptr, ps.row_hdr as u32, ps.parts) }.expect("the rig's leader region has room for the row store");
                assert!(ls.merge_from(ps, &mut rebase), "the rig's leader store holds every build row");
                ls.bits()
            });
            let l = unsafe { HashBuild::at_share(lead_ptr, BUILD_TABLE) };
            for e in p.entries() {
                let payload = match stored {
                    Some(bits) => (e.payload as u32).wrapping_add(rebase[HashBuild::partition_of(e.key, bits) as usize]) as i64,
                    None => e.payload,
                };
                assert!(l.insert_tagged(e.key, payload, e.used), "the rig's leader table holds every build entry");
            }
            return;
        }
        let partial = std::mem::take(&mut self.partial);
        self.lead().copy_from_slice(&partial);
        self.partial = partial;
    }

    pub fn ctx(&mut self) -> *mut Ctx {
        self.buf.as_mut_ptr() as *mut Ctx
    }

    /// One owner over the leader region: every partition of the phase-1 table (its table's records
    /// and its buffer chains) folded into a scratch table and reduced by the shape's functions,
    /// the round closed, a second round over the owner's round-2 buffers, the `Owned` header written.
    fn fold_owned(&mut self, fns: &gravity_abi::breaker::OwnerFns) {
        use gravity_abi::breaker::{HashAgg, Owned, OwnerTop, Rec, T1, HASHAGG_HDR, OWNED_HDR, TABLE_OWNED};
        let src = self.partial.as_ptr() as *const u8;
        let t1 = unsafe { T1::view(src) };
        let (pbits, layout) = (t1.pbits, t1.layout);
        let leader_len = self.lead().len() * 8;
        let leader_ptr = self.lead().as_mut_ptr() as *mut u8;
        let region = unsafe { leader_ptr.add(OWNED_HDR) };
        let bytes = leader_len - OWNED_HDR;
        let params = fns.params.as_ptr();
        assert!(unsafe { (fns.open)(params, region, bytes) } >= 0, "the rig's owner region opens");
        let site = unsafe { gravity_abi::ctx::site_ptr(self.ctx(), 0) };
        let fold_into = |records: &mut dyn FnMut(&mut dyn FnMut(Rec<'_>))| -> Vec<u64> {
            let mut n = 0usize;
            records(&mut |_| n += 1);
            let cap = (n * 2).max(64).next_power_of_two() as u32;
            let mut buf = vec![0u64; HashAgg::bytes(&layout, cap) as usize / 8];
            let t = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, &layout, cap, HashAgg::bytes(&layout, cap)) };
            records(&mut |r| t.find_or_insert(r.keys()).expect("room").fold(&r).expect("no float overflow"));
            buf
        };
        let rec = layout.size() as usize;
        for part in 0..1u32 << pbits {
            let mut buf = fold_into(&mut |f: &mut dyn FnMut(Rec<'_>)| {
                for r in t1.records() {
                    if r.route().part(pbits as u32) == part {
                        f(r);
                    }
                }
                for b in t1.chain(part) {
                    for i in 0..b.recs {
                        f(unsafe { Rec::at(b.ptr.add(i * rec) as *mut u8, &layout) });
                    }
                }
            });
            let t = buf.as_mut_ptr() as *const HashAgg;
            if unsafe { (*t).len } > 0 {
                assert_eq!(unsafe { (fns.reduce)(params, region, t, site, core::ptr::null(), -1) }, 0, "the rig's owner reduce");
            }
        }
        assert_eq!(unsafe { (fns.end_round)(params, region) }, 0, "the rig's round closes");
        if fns.rounds == 2 {
            let off = unsafe { OwnerTop::view(region) }.round2_off as usize;
            if off != 0 {
                let r2 = unsafe { T1::view(region.add(off)) };
                let (pbits2, layout2) = (r2.pbits, r2.layout);
                let rec2 = layout2.size() as usize;
                for q in 0..1u32 << pbits2 {
                    let mut n = 0usize;
                    for b in r2.chain(q) {
                        n += b.recs;
                    }
                    let cap = (n * 2).max(64).next_power_of_two() as u32;
                    let mut buf = vec![0u64; HashAgg::bytes(&layout2, cap) as usize / 8];
                    let t = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, &layout2, cap, HashAgg::bytes(&layout2, cap)) };
                    for b in r2.chain(q) {
                        for i in 0..b.recs {
                            let r = unsafe { Rec::at(b.ptr.add(i * rec2) as *mut u8, &layout2) };
                            t.find_or_insert(r.keys()).expect("room").fold(&r).expect("no float overflow");
                        }
                    }
                    if t.len > 0 {
                        assert!(unsafe { (fns.reduce2)(params, region, t as *const HashAgg, site, core::ptr::null(), -1) } >= 0, "the rig's round-2 reduce");
                    }
                }
            }
        }
        let _ = HASHAGG_HDR;
        unsafe { *(leader_ptr as *mut Owned) = Owned { nowners: 1, owner_bytes: bytes as u32, limit: leader_len as u32, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    }

    /// A fresh call rewinds the arena window to the scratch region, as the driver does.
    fn arm(&mut self) {
        let ctx = self.ctx();
        unsafe {
            if (*ctx).resume == 0 {
                (*ctx).arena_cur = self.scratch.as_mut_ptr() as *mut u8;
                (*ctx).arena_end = (self.scratch.as_mut_ptr() as *mut u8).add(self.scratch.len() * 8);
            }
        }
    }

    pub fn call(&mut self) -> i64 {
        self.arm();
        let ctx = self.ctx();
        unsafe { self.program.code().call(ctx) }
    }

    pub fn ret(&mut self) -> Ret {
        Ret::from_i64(self.call())
    }

    pub fn call_pipeline(&mut self, p: usize) -> Ret {
        self.arm();
        let ctx = self.ctx();
        Ret::from_i64(unsafe { self.program.code_of(p).call(ctx) })
    }

    pub fn set_claim(&mut self, lo: u64, hi: u64) {
        let ctx = self.ctx();
        unsafe { (*ctx).claim = gravity_abi::MorselRange::new(lo, hi) };
    }

    /// The driver's section end (`Pipeline.finish`, item 20): a finishing pipeline that ended `Done` is called
    /// once more with `SECTION_END` as its claim, before its partial is folded; the claim is restored after.
    fn section_end(&mut self, p: usize, rets: &[Ret]) {
        if !self.program.pipelines[p].finish || !matches!(rets.last(), Some(Ret::Done)) {
            return;
        }
        let ctx = self.ctx();
        let claim = unsafe { core::mem::replace(&mut (*ctx).claim, gravity_abi::MorselRange::SECTION_END) };
        unsafe { (*ctx).resume = 0 };
        self.host.section_ends += 1;
        assert_eq!(self.call_pipeline(p), Ret::Done, "pipeline {p}: the section end");
        unsafe { (*self.ctx()).claim = claim };
    }

    pub fn run_pipelines(&mut self) -> (Vec<Vec<Datum>>, Vec<Ret>) {
        self.run_pipelines_with(&owner_fns_for)
    }

    /// `run_pipelines` with the owner phase bound by `owner` (a lowered program's declared generic phase).
    pub fn run_pipelines_with(&mut self, owner: &dyn Fn(&Program, &[Datum]) -> Option<gravity_abi::breaker::OwnerFns>) -> (Vec<Vec<Datum>>, Vec<Ret>) {
        let mut rets = Vec::new();
        let n = self.program.pipelines.len();
        for p in 0..n {
            self.aim(p);
            loop {
                unsafe { (*self.ctx()).resume = if matches!(rets.last(), Some(Ret::Suspended { .. })) { (*self.ctx()).resume } else { 0 } };
                let r = self.call_pipeline(p);
                rets.push(r);
                assert_eq!(self.out_count(), 0, "a kernel returned with rows still buffered");
                if !matches!(r, Ret::Suspended { .. }) {
                    break;
                }
                assert!(p + 1 == n || gravity_abi::ctx::resume::resumable(self.resume()), "only the last pipeline, or an emit phase, may suspend");
            }
            self.section_end(p, &rets);
            if self.program.pipelines[p].produces().is_some() && matches!(rets.last(), Some(Ret::Done)) {
                self.fold_partial(owner);
            }
        }
        (self.host.pushed.clone(), rets)
    }

    /// `run_pipelines_with`, the owner chosen per pipeline, `after(rig, p)` run once pipeline `p`'s partial is folded
    /// (the lowerer's rung 8 lays the admitted set between its passes, as the runner does).
    pub fn run_pipelines_hooked(&mut self, owner: &dyn Fn(&Program, usize) -> Option<gravity_abi::breaker::OwnerFns>, after: &mut dyn FnMut(&mut Rig, usize)) -> (Vec<Vec<Datum>>, Vec<Ret>) {
        let mut rets = Vec::new();
        let n = self.program.pipelines.len();
        for p in 0..n {
            self.aim(p);
            loop {
                unsafe { (*self.ctx()).resume = if matches!(rets.last(), Some(Ret::Suspended { .. })) { (*self.ctx()).resume } else { 0 } };
                let r = self.call_pipeline(p);
                rets.push(r);
                if !matches!(r, Ret::Suspended { .. }) {
                    break;
                }
            }
            self.section_end(p, &rets);
            if self.program.pipelines[p].produces().is_some() && matches!(rets.last(), Some(Ret::Done)) {
                self.fold_partial(&|prog, _| owner(prog, p));
                after(self, p);
            }
        }
        (self.host.pushed.clone(), rets)
    }

    /// `run_pipelines_hooked` with pipeline `p0` driven claim by claim on the one partial (M3-L6M: `claims` fresh calls,
    /// `before(k)` before each — the rows that claim serves — then the section end and the fold), the rest as usual.
    pub fn run_pipelines_claimed(&mut self, p0: usize, claims: usize, before: &mut dyn FnMut(usize), owner: &dyn Fn(&Program, usize) -> Option<gravity_abi::breaker::OwnerFns>, after: &mut dyn FnMut(&mut Rig, usize)) -> (Vec<Vec<Datum>>, Vec<Ret>) {
        let mut rets = Vec::new();
        let n = self.program.pipelines.len();
        for p in 0..n {
            self.aim(p);
            if p == p0 {
                for k in 0..claims {
                    before(k);
                    unsafe { (*self.ctx()).resume = 0 };
                    let r = self.call_pipeline(p);
                    assert_eq!(r, Ret::Done, "pipeline {p}: claim {k}");
                    rets.push(r);
                }
            } else {
                loop {
                    unsafe { (*self.ctx()).resume = if matches!(rets.last(), Some(Ret::Suspended { .. })) { (*self.ctx()).resume } else { 0 } };
                    let r = self.call_pipeline(p);
                    rets.push(r);
                    if !matches!(r, Ret::Suspended { .. }) {
                        break;
                    }
                }
            }
            self.section_end(p, &rets);
            if self.program.pipelines[p].produces().is_some() && matches!(rets.last(), Some(Ret::Done)) {
                self.fold_partial(&|prog, _| owner(prog, p));
                after(self, p);
            }
        }
        (self.host.pushed.clone(), rets)
    }

    pub fn cell(&self, off: usize) -> u64 {
        self.buf[off / 8]
    }

    pub fn out_count(&self) -> usize {
        self.out.count as usize
    }

    pub fn resume(&self) -> i64 {
        self.cell(hdr::RESUME) as i64
    }

    pub fn state(&self, i: usize) -> u32 {
        unsafe { (*site_ptr(self.buf.as_ptr() as *mut Ctx, i)).state }
    }

    pub fn pos(&self, i: usize) -> ScanPos {
        unsafe { (*site_ptr(self.buf.as_ptr() as *mut Ctx, i)).pos }
    }

    pub fn run_all(&mut self) -> (Vec<Vec<Datum>>, Vec<Ret>) {
        let mut rets = Vec::new();
        loop {
            let r = self.ret();
            rets.push(r);
            assert_eq!(self.out_count(), 0, "a kernel returned with rows still buffered");
            if !matches!(r, Ret::Suspended { .. }) {
                return (self.host.pushed.clone(), rets);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<u64> {
        self.buf.clone()
    }

    pub fn changed_cells(&self, before: &[u64]) -> Vec<usize> {
        (0..self.buf.len()).filter(|&i| self.buf[i] != before[i]).map(|i| i * 8).collect()
    }

    pub fn site(&mut self, i: usize) -> &mut FakeSite {
        &mut self.host.sites[i]
    }

    pub fn pushes(&self) -> Vec<String> {
        self.host.calls.iter().filter(|c| c.starts_with("push")).cloned().collect()
    }

    pub fn windows(&self, i: usize) -> usize {
        self.host.calls.iter().filter(|c| **c == format!("window({i})")).count()
    }

    pub fn nexts(&self, i: usize) -> usize {
        self.host.calls.iter().filter(|c| **c == format!("next({i})")).count()
    }

    /// Pipeline `p` again as a fresh call after the program ran (the re-open rig, lane TESTS): every word the driver does not write
    /// before a fresh call filled with `poison` (each site's per-call cells `sel`..`part_facts`; the scratch arena when `scratch`, else zeroed), the header's per-call
    /// words written as the driver writes them (`flags` 0, `resume` RESUME_NONE: the fresh-call test, stop.rs); the sink stops after
    /// `stop` more rows when given. A suspend that leaves `resume` at RESUME_NONE or the poison forgot its position (asserted).
    pub fn rerun_poisoned(&mut self, p: usize, poison: u8, scratch: bool, stop: Option<usize>) -> (Vec<Vec<Datum>>, Vec<Vec<u8>>, Vec<Ret>) {
        use gravity_abi::ctx::{FLAG_RESUME, RESUME_NONE};
        let n0 = self.host.pushed.len();
        let word = u64::from_ne_bytes([poison; 8]);
        let (sel, len) = site::WINDOW_CELLS;
        for i in 0..self.program.sites.len() {
            unsafe { core::ptr::write_bytes((site_ptr(self.ctx(), i) as *mut u8).add(sel), poison, len) };
        }
        // unpoisoned, the scratch arena is zeroed as `Node::reset` zeroes it before a rescan
        self.scratch.fill(if scratch { word } else { 0 });
        self.aim(p);
        let saved = self.host.stop_after_rows;
        self.host.stop_after_rows = stop.map(|k| n0 + k);
        let mut rets = Vec::new();
        loop {
            let resumed = matches!(rets.last(), Some(Ret::Suspended { .. }));
            unsafe {
                (*self.ctx()).flags = if resumed { FLAG_RESUME } else { 0 };
                if !resumed {
                    (*self.ctx()).resume = RESUME_NONE;
                }
            }
            let r = self.call_pipeline(p);
            rets.push(r);
            if !matches!(r, Ret::Suspended { .. }) {
                break;
            }
            let w = unsafe { (*self.ctx()).resume };
            assert!(w != RESUME_NONE && w as u64 != word, "pipeline {p}: a suspend left RESUME at {w:#x} (its position never written)");
            if let Some(k) = stop {
                self.host.stop_after_rows = Some(self.host.pushed.len() + k);
            }
        }
        self.host.stop_after_rows = saved;
        (self.host.pushed[n0..].to_vec(), self.host.pushed_nulls[n0..].to_vec(), rets)
    }
}

pub fn allowed(t: &gravity_kernels::Touch) -> Vec<usize> {
    let mut v: Vec<usize> = t.hdr.to_vec();
    v.push(hdr::ARENA_CUR);
    for (s, cells) in t.sites {
        v.extend(cells.iter().map(|c| gravity_abi::ctx::site_off(*s) + c));
    }
    v.sort_unstable();
    v
}

pub fn subset(changed: &[usize], allowed: &[usize]) -> bool {
    changed.iter().all(|c| allowed.contains(c))
}

pub fn row(v: &[Datum]) -> Row {
    (v.to_vec(), vec![0; v.len()])
}

pub const SKIPPED: i64 = gravity_abi::status::WRITE_SKIPPED;
pub const RECHECK: i64 = gravity_abi::status::WRITE_RECHECK;
pub const LOCKED: i64 = WRITE_DONE;

/// The owner functions of a program whose first pipeline is a phase-1 table kernel (the runner's rule, in miniature).
pub fn owner_fns_for(program: &Program, params: &[Datum]) -> Option<gravity_abi::breaker::OwnerFns> {
    let mut words = [0u64; 16];
    let n = params.len().min(16);
    words[..n].copy_from_slice(&params[..n]);
    // the hand kernels exist only under the feature; without it no program runs one (the lib's own tests build the rig so)
    #[cfg(feature = "hand-kernels")]
    {
        use gravity_kernels::par_distinct;
        use gravity_kernels::table::params as par_hashagg;
        let is = |i: usize, f: gravity_program::Entry| program.pipelines.get(i).is_some_and(|_| program.code_of(i).entry() as usize == f as usize);
        if is(0, par_hashagg::par_hashagg) {
            return Some(par_hashagg::owner_fns(&words[..par_hashagg::NPARAMS]));
        }
        if is(0, par_distinct::par_distinct) {
            return Some(par_distinct::owner_fns(&words[..par_distinct::NPARAMS]));
        }
    }
    // an installed program's declared phase (milestone-2.md §2.6 item 12): the kernel's constructor over the words
    program.pipelines[0].owner.and_then(|d| d.ctor).map(|ctor| ctor(&words))
}

/// The two-pipeline program the emit tests (leader, heaptop) run pipeline 1 of: site 0 over `rel`'s `attnos` (`codes` a bit
/// per code lane, `ntables` table lanes after them), pipeline 0 a hash-agg share over `layout` (partition bits for `groups`) whose
/// kernel is never called (the test lays the owned region itself), pipeline 1 `emit` into the sink. It stands in for the
/// tier-3 group program these tests borrowed before the hand kernels were deleted.
#[allow(dead_code, clippy::too_many_arguments)]
pub fn emit_rig_program(rel: u32, attnos: &[i16], codes: u32, ntables: usize, layout: &gravity_abi::breaker::RecordLayout, out: &[gravity_program::OutCol], out_rows: u32, groups: u64, emit: gravity_program::Entry) -> Program {
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
    use gravity_program::{Code, End, LaneDesc, Layout, Pipeline, Reason, SiteDesc, WritePlan};
    unsafe extern "C-unwind" fn never(_: *mut gravity_abi::Ctx) -> i64 {
        unreachable!("the emit rig never runs its scan pipeline")
    }
    let st = gravity_program::Arena::new(16 << 10);
    let mut site = SiteDesc::range_source(st.handle(), rel, attnos);
    for k in 0..attnos.len() {
        if codes >> k & 1 != 0 {
            site = site.codes_lane(k);
        }
    }
    for i in 0..ntables {
        site = site.with_lane(LaneDesc::table(i as u16), false);
    }
    let bits = gravity_kernels::table::params::spill_bits(groups, layout.size());
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits } };
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(breaker), 0), Code::hand(never)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(emit)),
    ];
    let layout = Layout { nsites: 1, out_natts: out.len() as u16, out_rows, scratch_bytes: 0, exec_bytes: 0 };
    Program::with_pipelines(layout, &[site], out, WritePlan::SELECT, pipes)
}

/// The page protocol on the rig: the window's `take` rows (the lanes just published, by attribute id) formed as heap tuples on one
/// page — 24-byte page header, one line pointer per row, a 23-byte tuple header (`t_infomask2` = the relation's attribute count,
/// `HEAP_HASNULL` + a null bitmap when the row has a NULL), the data at each attribute's alignment (its length, the by-value rule) —
/// then `page`, `page_flags` (no null bitmap on the page; every tuple full — a row at a served index ≡ 11 mod 23 is formed short, its last
/// attribute absent, so the window takes the lane form) and the offset lane (1-based, width 2) at lane `natts`.
/// A lane of 512 datums, each the 1-byte-header numeric 0 (`[0x07, 0x00, 0x80]`: length 3, the short form, weight 0, dscale 0).
struct Poison([u64; 512]);
unsafe impl Sync for Poison {}
static ZERO_NUMERIC: [u8; 3] = [0x07, 0x00, 0x80];
static POISON: std::sync::LazyLock<Poison> = std::sync::LazyLock::new(|| Poison([ZERO_NUMERIC.as_ptr() as u64; 512]));

unsafe fn publish_page(site: &mut FakeSite, s: &mut Site, atts: &[(i16, u8, i16)], take: usize) {
    const HDR: usize = 24;
    const TUP_HDR: usize = 23;
    let natts = site.natts;
    let maxatt = atts.iter().map(|a| a.0).max().unwrap_or(0) as usize;
    let lanes: Vec<Lane> = site.lanes.clone();
    let lane_of = |attno: i16| atts.iter().find(|a| a.0 == attno).and_then(|a| (a.2 >= 0).then_some(a.2 as usize));
    let page = site.page_buf.as_mut_ptr() as *mut u8;
    let offs = site.page_offs.as_mut_ptr();
    core::ptr::write_bytes(page, 0, 8192);
    let mut upper = 8192usize;
    let mut nonulls = true;
    let mut full = true;
    for r in 0..take {
        // the row's cells by attribute: the lane's cell at r, valid per the lane's bitmap (null ptr = all valid)
        let cell = |attno: i16| -> (u64, bool) {
            match lane_of(attno) {
                Some(k) => {
                    let l = &lanes[k];
                    let v = *(l.ptr as *const u64).add(r);
                    let valid = l.valid.is_null() || (*(l.valid as *const u64).add(r / 64) >> (r % 64)) & 1 != 0;
                    (v, valid)
                }
                None => (0, true),
            }
        };
        let short = !site.page_clean && maxatt >= 2 && (site.served as usize + r) % 23 == 11;
        let tnatts = if short { maxatt - 1 } else { maxatt };
        full &= !short;
        // a varlena attribute (len 255 = typlen −1; no lane reads it) laid in every on-disk form by row: NULL (r ≡ 5 mod 13),
        // a 4-byte header at 4-alignment with a length whose low byte is zero (r ≡ 3 mod 8), an external pointer (r ≡ 7 mod 11),
        // else a 1-byte header of 1..7 bytes
        // a varlena a lane reads (a by-reference column) is laid as the lane's own datum: its image's bytes, 1-byte or 4-byte header
        let varlena_null = |r: usize| !site.page_clean && r % 13 == 5;
        let has_null = (1..=tnatts).any(|a| !cell(a as i16).1) || atts.iter().any(|&(a, len, k)| len == 255 && k < 0 && a as usize <= tnatts && varlena_null(r));
        nonulls &= !has_null;
        let bitmap = if has_null { maxatt.div_ceil(8) } else { 0 };
        let hoff = (TUP_HDR + bitmap).div_ceil(8) * 8;
        let mut off = 0usize;
        let mut cells: Vec<(usize, Vec<u8>)> = Vec::new();
        for &(a, len, _) in atts.iter() {
            let (v, valid) = cell(a);
            if !valid || a as usize > tnatts {
                continue;
            }
            if len == 255 && lane_of(a).is_some() {
                let p = v as *const u8;
                let bytes: Vec<u8> = if *p & 1 == 1 {
                    core::slice::from_raw_parts(p, (*p >> 1) as usize).to_vec()
                } else {
                    off = off.div_ceil(4) * 4;
                    core::slice::from_raw_parts(p, (*(p as *const u32) >> 2) as usize).to_vec()
                };
                off += bytes.len();
                cells.push((off - bytes.len(), bytes));
                continue;
            }
            if len == 255 {
                if varlena_null(r) {
                    continue;
                }
                let bytes: Vec<u8> = if r % 8 == 3 && !site.page_clean {
                    off = off.div_ceil(4) * 4;
                    let mut b = 64u32.wrapping_shl(2).to_le_bytes().to_vec();
                    b.extend((0..60).map(|i| (r + i) as u8));
                    b
                } else if r % 11 == 7 && !site.page_clean {
                    let mut b = vec![0x01u8, 18];
                    b.extend((0..16).map(|i| (r * 3 + i) as u8));
                    b
                } else {
                    let n = 1 + r % 7;
                    let mut b = vec![((n + 1) << 1) as u8 | 1];
                    b.extend((0..n).map(|i| b'a' + ((r + i) % 26) as u8));
                    b
                };
                off += bytes.len();
                cells.push((off - bytes.len(), bytes));
                continue;
            }
            let len = len as usize;
            off = off.div_ceil(len) * len;
            cells.push((off, v.to_le_bytes()[..len].to_vec()));
            off += len;
        }
        let tlen = hoff + off;
        upper = (upper - tlen) & !7;
        let tup = page.add(upper);
        *(tup.add(18) as *mut u16) = tnatts as u16;
        *(tup.add(20) as *mut u16) = if has_null { 1 } else { 0 };
        *tup.add(22) = hoff as u8;
        if has_null {
            for &(a, len, _) in atts.iter() {
                if cell(a).1 && a as usize <= tnatts && !(len == 255 && lane_of(a).is_none() && varlena_null(r)) {
                    *tup.add(TUP_HDR + (a as usize - 1) / 8) |= 1 << ((a as usize - 1) % 8);
                }
            }
        }
        for (o, bytes) in cells {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), tup.add(hoff + o), bytes.len());
        }
        let lp = (upper as u32) | (1u32 << 15) | ((tlen as u32) << 17);
        *(page.add(HDR + r * 4) as *mut u32) = lp;
        *offs.add(r) = r as u16 + 1;
    }
    s.page = page as *const u8;
    s.page_flags = (nonulls as u16) * site::PAGE_NONULLS | (full as u16) * site::PAGE_FULL;
    if full {
        // as the heap host publishes a raw page (`fill_raw`): a by-reference lane is left unfilled — the program reads a fast page's
        // columns from its tuples; a read of the lane instead finds every datum a numeric 0 (`POISON`), never the row's value
        for &(_, len, k) in atts.iter() {
            if len == 255 && k >= 0 {
                (*s.lanes.add(k as usize)).ptr = POISON.0.as_ptr() as *const u8;
            }
        }
    }
    *s.lanes.add(natts) = Lane::byval(site.page_offs.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, gravity_program::LaneId::Offset.encode());
    if (natts as u16) >= s.nlanes {
        s.nlanes = natts as u16 + 1;
    }
}
