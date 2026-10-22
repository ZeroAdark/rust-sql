//! Road `body.versioned` (H06a, R3-L04): the window's facts pick the fast body. A source publishes what it knows of a
//! window (`WinConds`: no nulls in the lanes the chain reads, no selection, dense keys, a full heap page, per-code tables
//! bound clean: no row reads a fault table); the frame runs the fast body under the AND of the published facts, with `Env::body` saying which hold, and the general body (every
//! fact tested per row) otherwise. A fact the source leaves `None` is unknown: it neither guards nor holds. At most two
//! bodies per window (frame.rs `row_loop`). Budget 160 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{CollectIn, PgVec};

use crate::face::source::{Body, BodyImpl, BodyMech, WinConds};
use crate::frame::env::Env;

crate::strategies! {
    /// Window facts pick a no-null / no-sel / dense body.
    BODY_VERSIONED = 5, "body.versioned", Scan;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(BODY_VERSIONED, &BodyImpl(&VERSIONED))], ..crate::ops::NONE };

struct Versioned;
static VERSIONED: Versioned = Versioned;

impl BodyMech for Versioned {
    fn guard(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, f: &WinConds) -> Option<(Value, Body)> {
        let known: PgVec<'_, Value> = [f.nonulls, f.nosel, f.dense, f.page_full, f.clean].into_iter().flatten().collect_in(env.mcx);
        let g = crate::frame::frame::all(b, env, &known[..])?;
        let body = Body { nonulls: f.nonulls.is_some(), nosel: f.nosel.is_some(), dense: f.dense.is_some(), page_full: f.page_full.is_some(), codes: false, packed: false, clean: f.clean.is_some() };
        Some((g, body))
    }
}
