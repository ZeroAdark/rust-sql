//! Run carry / collapse gated on the runs word (pgrc: declared / zone order; heap: key order proven per claim unit by
//! heap_pages.rs, a failed unit takes the hash path). Hooks / levers: H06/H22 R3-L13. Budget 160 lines (files.tsv, +25
//! % gate). Owner M1-TABLES.
//!
//! A keyed end carries its open run in registers: the run's image words, its hash when the image is wider than a word
//! (hash_open.rs; one word's KeyHash is a multiply, cheaper at the close than a word carried through every row) and an
//! open flag, then the end's accumulators (`RunFold`). A window seeds the empty run of the zero image: a row equal to the
//! run steps it (a first row of that key opens the seed, as v2's M3-L6N), so no row tests the flag, only a close does; a
//! new key closes the run (one table upsert per run, never per row) and opens the next. The decision reads the window's
//! runs word, never a correlation estimate (v1 negative result 4): a window whose runs word is 0 (no run to collapse)
//! takes the straight road, one `row_into` per row with no compare; a source that publishes none keeps the compare, which
//! costs a word compare per row against a table probe per run. A run never spans windows: `window_end` closes it. An AB
//! road: forced off, the end folds every row straight (its own `row_into`). A constant-key window (`WinConds::const_key`,
//! V4R-19 A: the source's exact zone stats) steps every row after its first into the open run with no compare.
//!
//! Registers: road `run.collapse` (`RUN_COLLAPSE`) as `RunCarryImpl`.
use gravity_expr::emit::hash::{emit_image_eq, emit_key_hash, ImageValues};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::table::face::{RunCarry, RunCarryImpl, RunFold, TableDecl};
use crate::table::keyimg::{ImageForm, ImageWords};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::row::RowVals;

crate::strategies! {
    /// Run carry / collapse gated on the runs word (pgrc: declared / zone order; heap: key order proven per claim unit
    /// by heap_pages.rs, a failed unit takes the hash path).
    RUN_COLLAPSE = 42, "run.collapse", Keyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(RUN_COLLAPSE, &RunCarryImpl(&RUNS))], ..crate::ops::NONE };

struct Runs;
static RUNS: Runs = Runs;

/// Whether the carry holds the run's hash: `hash.runopen` over an image of `n > 1` words.
fn carries_hash(n: usize) -> bool {
    n > 1 && ImageForm::RunOpen.on()
}

/// The carry's parts: `(image words, hash when carried, open, accumulators)`.
fn split(carry: &[Value], n: usize) -> (&[Value], Option<Value>, Value, &[Value]) {
    let k = n + carries_hash(n) as usize;
    (&carry[..n], carries_hash(n).then(|| carry[n]), carry[k], &carry[k + 1..])
}

/// The run held in the carry closed into the table (the end's `close`), when one is open (under `RunFold::admits_runs`,
/// an admitted one: open word 1; a run its admission refused holds 2 and closes into nothing).
fn close_open(b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &[Value], n: usize, exact: bool, fold: &dyn RunFold) {
    let (words, h, open, accs) = split(carry, n);
    let (l_close, l_after) = (b.create_block(&[]), b.create_block(&[]));
    let is_open = match fold.admits_runs(env) {
        true => b.icmp(IntCond::Eq, open, env.one),
        false => b.icmp(IntCond::Ne, open, env.zero),
    };
    b.cond_br(is_open, (l_close, &[]), (l_after, &[]));
    b.switch_to(l_close);
    let img = ImageWords::plain(ImageValues { words: words.to_vec() }, exact); // alloc: Program
    let h = match h {
        Some(h) => crate::table::hash_open::at_close(b, &img, h),
        None => emit_key_hash(b, &img.words),
    };
    fold.close(b, env, &img, h, accs);
    b.br(l_after, &[]);
    b.switch_to(l_after);
}

impl RunCarry for Runs {
    /// The seed: the zero image's empty run (its hash when carried, as a row of that key would open it), not open.
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, t: &TableDecl, fold: &dyn RunFold) -> PgVec<'e, Value> {
        let n = t.image_words as usize;
        let mut v = mcx::pg_vec![in env.mcx; env.zero; n];
        if carries_hash(n) {
            let seed = ImageWords::plain(ImageValues { words: v.to_vec() }, true); // alloc: Program
            v.push_in(crate::table::hash_open::at_open(b, &seed, env.zero));
        }
        v.push_in(env.zero);
        v.extend_in(fold.init(b, env));
        v
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _t: &TableDecl, win: &Win, row: &RowVals, img: &ImageWords, carry: &[Value], fold: &dyn RunFold) -> PgVec<'e, Value> {
        let n = img.words.words.len();
        let types: PgVec<'_, Type> = carry.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types[..]);
        let (words, h0, _, accs) = split(carry, n);
        let (l_cmp, l_straight) = (b.create_block(&[]), b.create_block(&[]));
        match win.facts.runs {
            Some(runs) => {
                let none = b.icmp(IntCond::Eq, runs, env.zero);
                b.cond_br(none, (l_straight, &[]), (l_cmp, &[]));
            }
            None => b.br(l_cmp, &[]),
        }

        // the straight road: every row folded into the table (the window has no runs to collapse)
        b.switch_to(l_straight);
        let h = emit_key_hash(b, &img.words);
        fold.row_into(b, env, row, img, h);
        b.br(l_join, carry);

        // the carry: a row of the run steps its accumulators (the seed's first row opens it), another key closes the run
        // and opens its own
        b.switch_to(l_cmp);
        let (l_same, l_new) = (b.create_block(&[]), b.create_block(&[]));
        if let Some((ck, _)) = win.facts.const_key {
            // V4R-19 (A): the window's one key is its open run's (opened by the window's first row): no compare
            let (l_ck, l_eq) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(ck, (l_ck, &[]), (l_eq, &[]));
            b.switch_to(l_ck);
            let open = split(carry, n).2;
            let is_open = b.icmp(IntCond::Ne, open, env.zero);
            b.cond_br(is_open, (l_same, &[]), (l_eq, &[]));
            b.switch_to(l_eq);
        }
        let same = emit_image_eq(b, &ImageValues { words: words.to_vec() }, &img.words); // alloc: Program
        b.cond_br(same, (l_same, &[]), (l_new, &[]));
        b.switch_to(l_same);
        let admits = fold.admits_runs(env);
        if admits {
            fires::fire("run.admit"); // shapecheck fire signal (D3326): a run's key admitted at its open, before its rows step
            // a refused run's row steps nothing; the seed's (open 0) is a new key, admitted at its open
            let open = split(carry, n).2;
            let (l_step, l_not) = (b.create_block(&[]), b.create_block(&[]));
            let stepping = b.icmp(IntCond::Eq, open, env.one);
            b.cond_br(stepping, (l_step, &[]), (l_not, &[]));
            b.switch_to(l_not);
            let seed = b.icmp(IntCond::Eq, open, env.zero);
            b.cond_br(seed, (l_new, &[]), (l_join, carry));
            b.switch_to(l_step);
        }
        let stepped = fold.step(b, env, row, accs);
        let mut next = mcx::to_vec_in(words, env.mcx);
        next.extend_in(h0);
        next.push_in(env.one);
        next.extend_in(stepped);
        b.br(l_join, &next[..]);
        b.switch_to(l_new);
        close_open(b, env, carry, n, img.exact, fold);
        if admits {
            // the new key's run, tested before any of its rows step: refused, it holds open word 2 and the old registers
            let (l_open, l_refused) = (b.create_block(&[]), b.create_block(&[]));
            let ok = fold.admit(b, env, img);
            b.cond_br(ok, (l_open, &[]), (l_refused, &[]));
            b.switch_to(l_refused);
            let mut refused = mcx::to_vec_in(&img.words.words[..], env.mcx);
            refused.extend_in(h0);
            refused.push_in(b.iconst(Type::I64, 2));
            refused.extend_from_slice_in(accs);
            b.br(l_join, &refused[..]);
            b.switch_to(l_open);
        }
        let init = fold.init(b, env);
        let stepped = fold.step(b, env, row, &init);
        let mut next = mcx::to_vec_in(&img.words.words[..], env.mcx);
        if carries_hash(n) {
            next.push_in(crate::table::hash_open::at_open(b, img, env.zero));
        }
        next.push_in(env.one);
        next.extend_in(stepped);
        b.br(l_join, &next[..]);
        b.switch_to(l_join);
        mcx::to_vec_in(b.block_params(l_join), env.mcx)
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, carry: &[Value], fold: &dyn RunFold) {
        close_open(b, env, carry, t.image_words as usize, true, fold);
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::agg::testkit::{fold, want, Case, CONST_KEY};
    use crate::table::bypass::HASHED;
    use super::RUNS;

    /// Runs of 7 equal keys over 13 keys in windows of 100 rows (a run cut by a window closes there): the carried road,
    /// the gated road (runs word set) and the straight road (runs word 0) fold every row into its key. Key 0 opens every
    /// 13th run and the first of each window's first part (the one-word zero image: its row opens the seed); the code form's
    /// two-word images carry their hash, the one-word images hash at the close.
    #[test]
    fn runs_fold_every_row_on_the_carried_the_gated_and_the_straight_road() {
        let key: fn(i64) -> i64 = |i| (i / 7) % 13;
        for (runs, code) in [None, Some(1), Some(0)].into_iter().flat_map(|r| [(r, false), (r, true)]) {
            let (got, _) = fold(Case { n: 5000, win: 100, key, table: &HASHED, run: Some(&RUNS), runs, code, null: 0, dense: None });
            assert_eq!(got, want(5000, key, 0), "runs word {runs:?}, code form {code}");
        }
    }

    /// V4R-19 (A): windows of 100 rows whose key is constant fold whole into one run each under `const_key`; a key that
    /// changes inside a window the source wrongly calls constant lands in the window's first key (the compare is gone).
    #[test]
    fn a_constant_key_window_folds_into_its_first_rows_run_with_no_compare() {
        let flat: fn(i64) -> i64 = |i| i / 100 % 17;
        for code in [false, true] {
            let (got, _) = fold(Case { n: 5000, win: 100, key: flat, table: &HASHED, run: Some(&RUNS), runs: Some(CONST_KEY), code, null: 0, dense: None });
            assert_eq!(got, want(5000, flat, 0), "code form {code}");
        }
        let mixed: fn(i64) -> i64 = |i| i / 50 % 17;
        let (got, _) = fold(Case { n: 5000, win: 100, key: mixed, table: &HASHED, run: Some(&RUNS), runs: Some(CONST_KEY), code: false, null: 0, dense: None });
        assert_eq!(got, want(5000, |i| i / 100 * 2 % 17, 0), "the shortcut is live");
    }
}
