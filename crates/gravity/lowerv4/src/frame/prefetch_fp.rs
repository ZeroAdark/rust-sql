//! Road `prefetch.fp` (H04, R4-L18; KEEP lowerv2-final text/prefetch.rs, the ladder's textprefetch.rs, D058): a text key
//! served as a dictionary code closes its run by reading the code's random `Fp128` entry, then the record's home slot:
//! two dependent misses. The row loop hints each code key's entry `KEY_PF` rows ahead (clamped to the window's last row):
//! in a body whose rows are lane rows (`Env::body.nosel`) that row, else the selection's entry for it (V4R-182: a filtered
//! scan with a text group key walks a selection, and its fp loads missed unhinted). A run opening hints
//! its home line and neighbour through its KeyHash (prefetch.rs `home`, never raw hash bits). The keyed ends call these
//! per row / per run open; each is a no-op where the road does not apply or is off. Budget 160 lines.
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntCond, Type, Value};

use gravity_expr::emit::hash::Bits;
use gravity_expr::emit::percode::code_at;

use crate::frame::env::Env;

crate::strategies! {
    /// Fp-entry prefetch in text-key run close.
    PREFETCH_FP = 39, "prefetch.fp", CodeKeyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(PREFETCH_FP, &())], ..crate::ops::NONE };

/// The distance in rows (the ladder's).
pub(crate) const KEY_PF: i64 = 16;

/// Hints the `Fp128` entries of the code keys `keys` (each its attno and its dictionary's Fp128 table in `Env::tables`)
/// `KEY_PF` rows past window row `j`.
pub(crate) fn ahead(b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value, keys: &[(i16, u8)]) {
    if keys.is_empty() || !env.applies(PREFETCH_FP) {
        return;
    }
    let Some(tables) = env.tables else { return };
    // a selected body gathers through its site's selection, and only when some key's code lane is the window's
    let sel = match env.body.nosel {
        true => None,
        false => match (env.site, keys.iter().any(|&(a, _)| env.codes.iter().any(|c| c.attno == a))) {
            (Some(k), true) => Some(k),
            _ => return,
        },
    };
    let Some(ja) = crate::frame::prefetch::row_ahead(b, env, j, KEY_PF) else { return };
    let ja = sel.map_or(ja, |k| sel_row(b, env, k, ja));
    for &(attno, t) in keys {
        let Some(c) = env.codes.iter().find(|c| c.attno == attno) else { continue };
        let lane = c.lane;
        let code = code_at(b, &lane, ja);
        let e = crate::rep::code::hash::fp_entry(b, tables, t, code);
        b.prefetch(e, 0);
    }
}

/// Selection entry `ja` of site `k`'s window (its lane row), or `ja` itself when the window has no selection (the site's
/// `SEL` word null; frame/site.rs `row_index` reads the same words). `ja` is clamped under the window's end, which is the
/// selection's length when it has one.
fn sel_row(b: &mut FunctionBuilder<'_>, env: &Env, k: usize, ja: Value) -> Value {
    let sel = b.load(Type::Ptr, env.ctx, (site_off(k) + site::SEL) as i32);
    let si = b.conv(ConvOp::PtrToInt, Type::I64, sel);
    let none = b.icmp(IntCond::Eq, si, env.zero);
    let (l_gather, l_row) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(none, (l_row, &[ja]), (l_gather, &[]));
    b.switch_to(l_gather);
    let at = b.ptr_add(sel, ja, 4, 0);
    let i = b.load_zext(Type::I32, at, 0);
    b.br(l_row, &[i]);
    b.switch_to(l_row);
    b.block_params(l_row)[0]
}

/// A text-key run opening with KeyHash word `h`: its home slot and neighbour hinted in `table` (prefetch.rs `home`).
#[expect(dead_code, reason = "no keyed run opening calls it yet (lowerv2-final hinted the text-key home slot, D058)")]
pub(crate) fn home_at_open(b: &mut FunctionBuilder<'_>, env: &mut Env, h: Value, table: Value, cap_bits: Bits, used: Bits, rec: u32) {
    if env.applies(PREFETCH_FP) {
        crate::frame::prefetch::home(b, env, h, table, cap_bits, used, rec, None);
    }
}
