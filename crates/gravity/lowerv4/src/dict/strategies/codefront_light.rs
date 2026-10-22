//! Road `code.front_light` (cards/M3.md L8, H23; V4R-184): the count front, the ladder's rung-3 text front
//! (gravity_lower/text.rs at ec2df04e1fc). Owner M3-LEVERS-A. Budget 250 lines.
//!
//! A count top keyed by one code (`GROUP BY <text> ORDER BY count(*) DESC`) paid, per row,
//! the key's Fp128 image load (a random 16-byte entry of the part's table), its hash and a probe or a bypass append; the
//! owner merge then folded one record per row past code.front_part's 256 KiB front (V4R-174's two designs kept that image
//! load and full-width records, and lost to road-off: D512). Here the row loop reads only the key's code: `cnt[code] += 1`
//! over the part the claim reads, the code appended to the touched list when its count was 0 (a `u32` each, no branch).
//! The part's counts become one record per touched code in the claim's hashed table (the Fp128 image, the count, the
//! representative `part << 32 | code`) once per part: when a window of another part binds (`bind`, per window before its
//! rows) and at the claim's end. The owner merge sees one record format, as from the row path.
//!
//! Takes (`plan`): a table of code keys over the hashed table whose every slot is a row count (`count(*)`, `count(key)`
//! of any key, the count of a constant that is not NULL or of a NOT NULL column (count(*)'s register): no FILTER, DISTINCT,
//! order or vector form) or a key's MinCode
//! representative; a record of at most `CODE_FRONT_MAX_RECORD` bytes. Two or more keys are taken when the planner's
//! group estimate fits `LIGHT_KEYS_MAX_GROUPS` (the cost home's): a table estimated past it would pay
//! the index on every row of every part its bind refuses. They count by the index `sum code_k * stride_k` over the bound
//! part's dictionary sizes (`stride_k` the product of the earlier keys' sizes), one count a tuple of the part's
//! dictionaries. A NULL key (its code all ones, past every size), a part whose dictionary sizes' product passes
//! `LIGHT_MAX_CODES` or the window it binds at's rows over `LIGHT_ROWS_A_COUNT` (the cost home's `light_takes`), a part
//! after one the front counted at fewer rows a touched count (a filter thinned them: the convert declines the front for
//! the statement's later parts in that worker, a frame word kept across its claims, since a claim never crosses a part's
//! edge) and a claim whose arena refuses the arrays take the hashed table per row (today's path); the row loop tests the
//! capacity before it forms the index. One key: a part past `LIGHT_MAX_CODES` and a claim of too few rows to fill the
//! part's arrays (the cost home's `light_claim_takes`) take the hashed table.
//! Memory: 8 bytes a code of the largest bound dictionary, at most 8 MiB a worker, from the claim's arena (the statement's
//! grant), gone with the claim. Hosts: front_light_bind, front_light_flush (kernels host_abi v4 m3_light.rs).
//!
//! Registers: road `code.front_light` (`CODE_FRONT_LIGHT`) as a `CountFrontImpl` (face/keyed.rs `count_front`).
use gravity_abi::breaker::{flag_words, light_ways_max_codes, SlotKind, LAYOUT_IMPLICIT_COUNT, LIGHT_KEYS_MAX_GROUPS, LIGHT_WAYS};
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::ops::shared::agg::{GroupDecl, Register, NONE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::dict::strategies::front_light::{front_light_bind_keys, front_light_flush_keys, LIGHT_KEYS_MAX, LK_BYTES, LK_DECLINED_BYTES, LK_LANE, LK_NEXT, LK_REP, LK_SIZE, LK_STRIDE};
use gravity_kernels::ops::sort::entries as hosts;
use gravity_kernels::host_abi::BIND_OUT_WORDS;
use gravity_program::cost::CODE_FRONT_MAX_RECORD;
use gravity_program::DictRef;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::table::face::{CountFrontImpl, CountFrontMech, CountKey, CountKeyShape, CountShape, TableDecl};
use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::px::{Px, WordKey};
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::CellRef;

crate::strategies! {
    /// Heavy / light bound code front with a light tag, flushed per part dictionary.
    CODE_FRONT_LIGHT = 77, "code.front_light", CodeKeyedTopk;
}

pub(crate) const ROWS: Table = Table { strategies: &[(CODE_FRONT_LIGHT, &CountFrontImpl(&LIGHT))], ..crate::ops::NONE };

struct Light;
static LIGHT: Light = Light;

/// KEYED's frame word of pipeline `k` that keeps a front of two or more keys' decline (past the group emit's words,
/// table/emit_group.rs `W_*`).
const W_DECLINED: u8 = 16;

/// `c` with its decline's frame word when it counts two or more keys. A claim never crosses a part's edge (the claim
/// cursor's bounds), so the convert's decline in the end's state, which every claim's open clears, never reached a later
/// part: the frame word (zero at the statement's bind, the worker's own) keeps it across the statement's claims; each
/// claim's open copies it into the state's decline word and its finish copies it back.
pub(crate) fn with_declined<'g>(px: &mut Px<'_, '_>, k: usize, mut c: CountShape<'g>) -> CountShape<'g> {
    if c.keys.len() > 1 {
        c.declined = Some(px.frame_word(WordKey::Open { face: Face::Keyed, k, j: u64::MAX, i: W_DECLINED }) as i32 * 8);
    }
    c
}

/// Frame word `off` of `env`'s words site.
fn words(env: &Env, off: Option<i32>) -> Option<(Value, i32)> {
    env.words.zip(off)
}

fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> gravity_ir::HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// A negative host status leaves through the exit.
fn fail_on(b: &mut FunctionBuilder<'_>, env: &Env, x: &Exits, r: Value) {
    let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
    b.cond_br(bad, (x.l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
}

/// The hosts' `desc` word (m3_light.rs `LD_*`): one key: the slots, the image words, the code lane, the end's site and
/// `LD_WAYS` when its row loop tests the ways; two or more: the count slot, the image words, the keys, the site (each
/// key's lane and representative are its entry's).
fn desc(b: &mut FunctionBuilder<'_>, env: &Env, c: &CountShape) -> Value {
    let Some(s) = env.site else { crate::refuse!(env.node, Reason::Bug { payload: "a count front outside a site" }) };
    let w = match &c.keys[..] {
        [k] => c.count as u64 | k.rep.map_or(hosts::LD_NONE, u64::from) << 8 | (c.nkeys as u64) << 16 | (k.lane as u64) << 24 | (s as u64) << 40 | if c.ways { hosts::LD_WAYS } else { 0 },
        ks => c.count as u64 | hosts::LD_NONE << 8 | (c.nkeys as u64) << 16 | (ks.len() as u64) << 24 | (s as u64) << 40,
    };
    b.const_raw(Type::I64, w)
}

/// Key `k`'s entry in the words (a front of two or more keys).
fn entry(k: usize) -> i32 {
    hosts::LW_BYTES as i32 + k as i32 * LK_BYTES as i32
}

/// Key cell `key`'s code word, all ones for a NULL.
fn key_code(b: &mut FunctionBuilder<'_>, env: &mut Env, key: CellRef<'_>) -> Value {
    let mut cx = env.lx(b, env.node);
    let code = crate::rep::code_word(&mut cx, key);
    env.memo = cx.memo;
    key.null.map_or(code, |n| {
        let ones = b.iconst(Type::I64, -1);
        b.select(n, ones, code)
    })
}

/// `e` is the key's own column of the producer's row (a relabel stripped).
fn is_key(e: Node<'_>, col: i16) -> bool {
    e.as_relabel_type().map_or(e, |r| r.arg).as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == col)
}

/// `e` is a constant that is not NULL (a relabel stripped): its count is the rows' count.
fn never_null(e: Node<'_>) -> bool {
    e.as_relabel_type().map_or(e, |r| r.arg).as_const().is_some_and(|c| !c.constisnull)
}

impl CountFrontMech for Light {
    fn plan<'g>(&self, t: &TableDecl, g: &GroupDecl<'_>, keys: &[CountKey], mcx: Mcx<'g>) -> Option<CountShape<'g>> {
        let (l, n) = (&t.layout, keys.len());
        // the image: two words a key (its Fp128 entry), then the flags words when a key is nullable
        let words = (2 * n..=2 * n + flag_words(n)).contains(&(l.nkeys() as usize));
        let estimated = n == 1 || g.groups <= LIGHT_KEYS_MAX_GROUPS;
        if l.is_wide() || l.flags & LAYOUT_IMPLICIT_COUNT != 0 || l.size() > CODE_FRONT_MAX_RECORD || n == 0 || n > LIGHT_KEYS_MAX || !estimated || g.keys.len() != n || keys.iter().any(|k| k.fp_table == DictRef::NO_TABLE) || !words {
            return None;
        }
        let reps = || g.keys.iter().map(|k| k.rep);
        let last = reps().all(|r| r != NONE).then(|| reps().max()).flatten();
        let mut count = None;
        for j in 0..l.nslots() {
            match l.slot_kind(j) {
                SlotKind::MinCode if reps().any(|r| j == r as usize) => {}
                SlotKind::Count if count.is_none_or(|c| c == j) && g.aggs.iter().any(|a| a.slot as usize == j) => count = Some(j),
                SlotKind::MaxI64 | SlotKind::SumF64 if count.is_some() && last.is_some_and(|r| j > r as usize) && g.aggs.iter().all(|a| a.slot as usize != j) => {} // W9 PERF C5: a top's order slot (keyed/order_slot.rs), the emit's alone: clear in a flushed record as in the row path's
                _ => return None,
            }
        }
        let count = count?;
        for a in g.aggs.iter() {
            let m = a.mods;
            if a.slot as usize != count || a.filter.is_some() || m.distinct != 0 || m.order != 0 || m.direct != 0 || a.count_slot != NONE || a.dscale_slot != NONE {
                return None;
            }
            let args = &a.node.as_aggref()?.args;
            let one = args.first().filter(|_| args.len() == 1).map(|te| te.as_target_entry().map_or(te, |t| t.expr));
            // count(x) of a column a valid NOT NULL constraint guards carries count(*)'s register (normalize's count_star)
            if !(a.reg == Register::CountStar || args.is_nil() || one.is_some_and(|x| never_null(x) || keys.iter().any(|k| is_key(x, k.col)))) {
                return None;
            }
        }
        let mut shapes = mcx::reserved_in(n, mcx);
        for (k, r) in keys.iter().zip(reps()) {
            shapes.push_in(CountKeyShape { lane: k.lane, fp_table: k.fp_table, rep: if r == NONE { None } else { Some(u8::try_from(r).ok()?) } });
        }
        let keys = shapes;
        fires::fire("dict.front_light"); // shapecheck fire signal (REGISTRY KEYED-12, B3-09)
        // D3159: the ways only count a part of at most `light_ways_max_codes()` codes (one thread's L1d of this host), so a
        // key the planner estimates past that keeps trunk's row loop and never counts in ways (hot-nc G026, gt's title: ~12 %
        // slower at W0 with them)
        let ways = n == 1 && g.groups <= light_ways_max_codes();
        if ways {
            fires::fire("dict.front_light_ways");
        }
        Some(CountShape { count: count as u8, nkeys: l.nkeys() as u8, keys, declined: None, ways })
    }

    fn state_bytes(&self, c: &CountShape) -> u32 {
        hosts::LW_BYTES + if c.keys.len() > 1 { c.keys.len() as u32 * LK_BYTES + LK_DECLINED_BYTES } else { 0 }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: Value, c: &CountShape) {
        for off in (0..hosts::LW_BYTES as i32).step_by(8) {
            b.store(w, off, env.zero);
        }
        let none = b.const_raw(Type::I64, hosts::LW_NO_PART);
        b.store(w, hosts::LW_PART, none);
        if c.keys.len() > 1 {
            for (k, key) in c.keys.iter().enumerate() {
                for off in (0..LK_BYTES as i32).step_by(8) {
                    b.store(w, entry(k) + off, env.zero);
                }
                let (lane, rep) = (b.const_raw(Type::I64, key.lane as u64), b.const_raw(Type::I64, key.rep.map_or(hosts::LD_NONE, u64::from)));
                b.store(w, entry(k) + LK_LANE, lane);
                b.store(w, entry(k) + LK_REP, rep);
            }
            // the convert's decline word after the keys: the statement's so far (its frame word)
            let v = words(env, c.declined).map_or(env.zero, |(ws, off)| b.load(Type::I64, ws, off));
            b.store(w, entry(c.keys.len()), v);
        }
    }

    fn bind(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value, w: Value, c: &CountShape) {
        let (Some(s), Some(tables)) = (env.site, env.tables) else { crate::refuse!(env.node, Reason::Bug { payload: "a count front without its window's tables" }) };
        let part = b.load_zext(Type::I32, env.ctx, (site_off(s) + site::PART_ID) as i32);
        let cur = b.load(Type::I64, w, hosts::LW_PART);
        let same = b.icmp(IntCond::Eq, part, cur);
        let (l_bind, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bind);
        b.cond_br(same, (l_go, &[]), (l_bind, &[]));
        b.switch_to(l_bind);
        let tables = if b.value_type(tables) == Type::Ptr { tables } else { b.conv(ConvOp::IntToPtr, Type::Ptr, tables) };
        let r = match &c.keys[..] {
            [k] => {
                let fps = b.load(Type::Ptr, tables, (BIND_OUT_WORDS * 8) as i32 * k.fp_table as i32);
                let d = desc(b, env, c);
                let f = host(b, "front_light_bind", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::Ptr], gravity_abi::host_words!(hosts::front_light_bind(ctx, st, w, desc, part, fps)));
                b.call_host(f, &[env.ctx, st, w, d, part, fps], Some(Type::I64))
            }
            ks => {
                // each key's next table, which the host takes once the bound part's counts are converted
                for (k, key) in ks.iter().enumerate() {
                    let fps = b.load(Type::Ptr, tables, (BIND_OUT_WORDS * 8) as i32 * key.fp_table as i32);
                    b.store(w, entry(k) + LK_NEXT, fps);
                }
                let d = desc(b, env, c);
                let f = host(b, "front_light_bind_keys", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(front_light_bind_keys(ctx, st, w, desc, part)));
                b.call_host(f, &[env.ctx, st, w, d, part], Some(Type::I64))
            }
        }
        .expect("front_light_bind returns a status");
        let x = env.x;
        fail_on(b, env, &x, r);
        b.br(l_go, &[]);
        b.switch_to(l_go);
    }

    fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, words: &dyn Fn(&mut FunctionBuilder<'_>, &Env) -> Value, c: &CountShape, keys: &[CellRef<'_>], row_at: Value) -> Block {
        let w = words(b, env);
        let mcx = env.mcx;
        let codes: PgVec<'_, Value> = keys.iter().map(|&key| key_code(b, env, key)).collect_in(mcx);
        let (l_front, l_table, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let cap = b.load(Type::I64, w, hosts::LW_CAP);
        let code = match &codes[..] {
            [code] => *code,
            cs => {
                // a part the bind refused (cap 0: past the largest array, a sparse array, a refused draw) takes the table
                // before the index
                let (refused, l_index) = (b.icmp(IntCond::Eq, cap, env.zero), b.create_block(&[]));
                b.cond_br(refused, (l_table, &[]), (l_index, &[]));
                b.switch_to(l_index);
                // the index under the bound part's sizes (the bind's); a code at or past its size (a NULL's all ones) sends
                // the row past every capacity
                let (mut at, mut off) = (cs[0], None);
                for (k, &c) in cs.iter().enumerate() {
                    let n = b.load(Type::I64, w, entry(k) + LK_SIZE);
                    let past = b.icmp(IntCond::Uge, c, n);
                    off = Some(match off {
                        Some(o) => b.int_bin(IntBinOp::Or, o, past),
                        None => past,
                    });
                    if k > 0 {
                        let stride = b.load(Type::I64, w, entry(k) + LK_STRIDE);
                        let term = b.int_bin(IntBinOp::Mul, c, stride);
                        at = b.int_bin(IntBinOp::Add, at, term);
                    }
                }
                let ones = b.iconst(Type::I64, -1);
                b.select(off.expect("two or more keys"), ones, at)
            }
        };
        let ok = b.icmp(IntCond::Ult, code, cap);
        b.cond_br(ok, (l_front, &[]), (l_table, &[]));
        b.switch_to(l_front);
        let (cnt, list, n) = (b.load(Type::Ptr, w, hosts::LW_CNT), b.load(Type::Ptr, w, hosts::LW_TOUCHED), b.load(Type::I64, w, hosts::LW_N));
        let at = b.ptr_add(cnt, code, 4, 0);
        let v = b.load_zext(Type::I32, at, 0);
        // `touched[n] = code; n += (cnt[code] == 0)`: the entry is overwritten until a fresh code keeps it
        let e = b.ptr_add(list, n, 4, 0);
        let c32 = b.conv(ConvOp::Trunc, Type::I32, code);
        b.store(e, 0, c32);
        let fresh = b.icmp(IntCond::Eq, v, env.zero);
        let f = b.conv(ConvOp::Zext, Type::I64, fresh);
        let n1 = b.int_bin(IntBinOp::Add, n, f);
        b.store(w, hosts::LW_N, n1);
        let v1 = b.int_bin(IntBinOp::Add, v, env.one);
        let v1 = b.conv(ConvOp::Trunc, Type::I32, v1);
        b.store(at, 0, v1);
        b.br(l_done, &[]);
        b.switch_to(l_table);
        // D3159: a part the bind counts in ways (one key; it binds `cap` 0 and `ways` = its codes) counts row `row_at`
        // into way `row_at % LIGHT_WAYS` (`alloc` codes a way) and keeps no list: neither a repeated code's count nor `n`
        // chains one row's store into the next row's load. The test rides the row the list missed, so the list's row
        // code above is the same as a claim with no part in ways (G001 / G003 / G013 pay nothing per listed row). The
        // block derives its own words pointer and reloads the row's code (a NULL's all ones again): the ones the list
        // used, kept live past its compare, were spilled to the stack on every row of the filter's bit-scan loop (hot-nc
        // G004 / G005 / G006 / G010 / G027: about 0.6 ns a row, g15 / g17 on box 88). Only a shape whose key the planner
        // estimates within the ways' codes (`c.ways`, the bind's `LD_WAYS`) tests them.
        if let ([key], true) = (keys, c.ways) {
            let (w, code) = (words(b, env), key_code(b, env, *key));
            let (l_ways, l_rest) = (b.create_block(&[]), b.create_block(&[]));
            let ways = b.load(Type::I64, w, hosts::LW_WAYS);
            let in_ways = b.icmp(IntCond::Ult, code, ways);
            b.cond_br(in_ways, (l_ways, &[]), (l_rest, &[]));
            b.switch_to(l_ways);
            let (cnt, stride) = (b.load(Type::Ptr, w, hosts::LW_CNT), b.load(Type::I64, w, hosts::LW_ALLOC));
            let mask = b.iconst(Type::I64, (LIGHT_WAYS - 1) as i64);
            let way = b.int_bin(IntBinOp::And, row_at, mask);
            let off = b.int_bin(IntBinOp::Mul, way, stride);
            let idx = b.int_bin(IntBinOp::Add, off, code);
            let p = b.ptr_add(cnt, idx, 4, 0);
            let v = b.load_zext(Type::I32, p, 0);
            let v1 = b.int_bin(IntBinOp::Add, v, env.one);
            let v1 = b.conv(ConvOp::Trunc, Type::I32, v1);
            b.store(p, 0, v1);
            b.br(l_done, &[]);
            b.switch_to(l_rest);
        }
        l_done
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value, w: Value, c: &CountShape, x: &Exits) {
        let d = desc(b, env, c);
        let f = match c.keys.len() {
            1 => host(b, "front_light_flush", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], gravity_abi::host_words!(hosts::front_light_flush(ctx, st, w, desc))),
            _ => host(b, "front_light_flush_keys", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], gravity_abi::host_words!(front_light_flush_keys(ctx, st, w, desc))),
        };
        let r = b.call_host(f, &[env.ctx, st, w, d], Some(Type::I64)).expect("front_light_flush returns a status");
        fail_on(b, env, x, r);
        // the claim's decline (its last part's convert included) kept for the statement's later claims
        if let Some((ws, off)) = words(env, c.declined).filter(|_| c.keys.len() > 1) {
            let v = b.load(Type::I64, w, entry(c.keys.len()));
            b.store(ws, off, v);
        }
    }
}
