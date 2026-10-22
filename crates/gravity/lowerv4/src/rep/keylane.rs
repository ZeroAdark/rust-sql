//! Road `sort.keylane` (cards/M3.md L4, H26e): the producer's key lane. Every run row leads with one `[flags, word]` pair
//! per key (`pairs`: rep.word.order's image, or a C-ordered `name`'s first eight bytes, with DESC and NULLS FIRST / LAST
//! folded by `fold`; flags 0 a NULLS FIRST NULL, 1 a value, 2 a NULLS LAST NULL, the word 0 for a NULL so NULLs tie). When
//! the road applies (props `lane_key`: key 0 is one order word, its pair the whole key), the run's first row sets
//! `RUN_KEYLANE` in its `pad[2]` (natural.rs `mark`) and the hosts order the run by key 0's pair with an LSD radix, ties by
//! the full row compare (kernels host_abi/v4/m3_sort.rs). Forced off, the run is sorted by the compare alone. Budget 160
//! lines. Owner M3-SORT.
use gravity_decompose::ops::shared::sort::SortKey;
use gravity_decompose::catalog::typefacts::{FixedNorm, KeyColl, TypeFacts};
use gravity_ir::{HostFlags, IntBinOp, IntUnOp, Type, Value};
use gravity_program::Rep;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::list::NodeList;
use types_nodes::primnodes::OUTER_VAR;

use crate::face::kinds::KindId;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;
use crate::rep::{self, HashArm, Lx, Scope};

use crate::rep::word::order::{fold, name_image, Folded, NAME_BYTES};
use crate::face::runs::{Col, Form};
use gravity_kernels::host_abi::{VAR_WIDTH, WORD_WIDTH};

crate::strategies! {
    /// Producer key + row-index lane, one-word null image, short-key tie.
    SORT_KEYLANE = 71, "sort.keylane", SortKeylane;
}

pub(crate) const ROWS: Table = Table { strategies: &[(SORT_KEYLANE, &())], ..crate::ops::NONE };

pub(crate) use types_core::catalog::NAMEOID as NAME;

/// A sort key: its run column, its direction and NULLS placement, its collation, its tail's `SortSpec` entry (0: none), and
/// whether it orders by bpchar's operator (`SortKey::is_bpchar`: trailing blanks ignored, whatever its column's type), and
/// for a key ordered by its type's comparator (rep.hosted.order) the btree support function its tail calls (0: none).
#[derive(Clone, Copy)]
pub(crate) struct Key {
    pub col: u16,
    pub desc: bool,
    pub nulls_first: bool,
    pub coll: KeyColl,
    pub tail: u16,
    pub bpchar: bool,
    pub proc: u32,
    /// A comparator-ordered key of its type's default btree family (not D1346's `own_family`): a numeric one's prefix word
    /// is typesem's order word (`payload_word`: a numeric's, an inet / cidr's, a bit / varbit's), not the hosted constant 0.
    pub dflt: bool,
    /// setop_core-0129: an anonymous record key whose equal copies `record_cmp` cannot raise on (decompose `SortKey::eq_tie`):
    /// the table flags it `KFLAG_EQTIE` (face/runs.rs), so equal copies tie with no call.
    pub eq_tie: bool,
    /// USEROP: the key's comparator may be the user's (`cmp_proc`): the table flags it `KFLAG_USER` (face/runs.rs), so the
    /// run orders by PostgreSQL's qsort over the arrival order and calls the comparator on the pairs PostgreSQL's does,
    /// the first raise being PostgreSQL's.
    pub user: bool,
    /// TOTAL-ORDER: the key's comparator is a builtin's that reaches no user code (`cmp_proc`'s third answer, `total_cmp`):
    /// its order is total, so the table flags it `KFLAG_TOTAL` (face/runs.rs) and its run keeps std's sort; every other
    /// hosted key orders by PostgreSQL's qsort (kernels `Order::Lenient`), which never panics on an order that is not total.
    pub total: bool,
}

/// Key `k`'s type facts over column facts `col`: bpchar's when the key orders by bpchar's operator over a text / varchar
/// column (a merge join's side sorts `t USING <` of bpchar's when its clause relabels `t`, R2 13b), so its prefix word and
/// its tail's `KFLAG_BPCHAR` trim as `bpcharcmp` does; else the column's own.
pub(crate) fn key_tf(col: &TypeFacts, k: &Key) -> TypeFacts {
    match col.image.is_plain_text() {
        true if k.bpchar && col.typlen == -1 => *gravity_decompose::catalog::typefacts::row_of(BPCHAR).unwrap_or(col),
        _ => *col,
    }
}

use types_core::catalog::BPCHAROID as BPCHAR;
use types_core::catalog::{CIDROID as CIDR, INETOID as INET};

/// The keys as run columns and the run's `SortSpec`. A key's resno is a column of the sorted row through `tlist` (a Sort's
/// target list, its entries `OUTER_VAR`), or of the row itself when `None` (MergeAppend's keys). A key over a column with
/// an image tail (`tails[col]`: `(offset, width)`, a `name`'s 64-byte image or a var slot, `VAR_WIDTH`) compares the tail
/// after its word ties, reversed for DESC: keys 0-3 through the `SortSpec`, keys from 4 through their var table entries
/// (`Key::tail`, V4R-265: such a key makes a var run, `var_key`; past the kernels' `ROWCMP_KEYS` (`gravity_kernels::host_abi`) read from the table,
/// W8-SETOP-WIDEKEYS); every other key is decided by its word. `proc(key, col)`: a hosted key's comparator (runs/sort.rs),
/// 0 for any other key, whether it may reach user code (`cmp_proc`, `Key::user`), and whether it is a builtin's total order
/// (`Key::total`). USEROP / D2041: a key past `ROWCMP_KEYS` whose
/// comparator may reach user code serves like any other (`RowCmp::cmp_far` calls it by its table entry); its refusal
/// guarded only how often the comparator is called, and the user's ruling (2026-10-04) makes call counts free.
pub(crate) fn keys(node: i32, sk: &[SortKey], tlist: Option<&NodeList<'_>>, tails: &[Option<(u32, u16)>], proc: &dyn Fn(&SortKey, usize) -> (u32, bool, bool)) -> (Vec<Key>, u64) {
    let shape = |why| -> ! { crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }) };
    let mut keys = Vec::with_capacity(sk.len()); // alloc: Program
    let mut spec = 0u64;
    for (i, sk) in sk.iter().enumerate() {
        let Some(col) = key_col(sk, tlist, tails.len()) else { shape("a sort key that is not a column of the sorted row") };
        let mut tail = 0;
        if let Some((off, width)) = tails[col as usize] {
            if off / 8 > 255 { shape(if width == VAR_WIDTH { "a text sort key past the run row's slot reach" } else { "a name sort key past the run row's image reach" }) }
            tail = (off / 8) as u16 | width << 8 | (sk.desc as u16) << 15;
            if i < 4 {
                spec |= (tail as u64) << (16 * i);
            }
        }
        let (proc, user, total) = if tail != 0 { proc(sk, col as usize) } else { (0, false, false) };
        // D1346 / core-0241: text_pattern_ops' and bpchar_pattern_ops' keys come under "C", whose bytes order is theirs
        // (a bpchar's image trims its trailing blanks, as internal_bpchar_pattern_compare)
        if sk.compare_fn() && proc == 0 {
            shape("a sort key of a non-default btree family off its comparator (D1346)");
        }
        keys.push(Key { col, desc: sk.desc, nulls_first: sk.nulls_first, coll: sk.coll, tail, bpchar: sk.is_bpchar(), proc, dflt: proc != 0 && !sk.own_family, eq_tie: proc != 0 && sk.eq_tie, user: proc != 0 && user, total: proc != 0 && !user && total }); // alloc: Program
    }
    (keys, spec)
}

/// Key `sk`'s run column (0-based) among `ncols`: its resno through `tlist` (a Sort's target list, its entries `OUTER_VAR`), or
/// the row's own when `None` (MergeAppend's keys); none for a key that is no column of the sorted row.
pub(crate) fn key_col(sk: &SortKey, tlist: Option<&NodeList<'_>>, ncols: usize) -> Option<u16> {
    let attno = match tlist {
        Some(l) => l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == sk.resno).and_then(|te| te.expr.as_var()).filter(|v| v.varno == OUTER_VAR).map(|v| v.varattno),
        None => Some(sk.resno),
    };
    attno.filter(|&a| a >= 1 && a as usize <= ncols).map(|a| a as u16 - 1)
}

/// USEROP: each run column some key orders by its family's comparator alone (`SortKey::compare_fn`, the CompareFn key kind):
/// `tails` gives such a column its comparator's argument as its tail whatever its type.
pub(crate) fn cmp_cols<'m>(sk: &[SortKey], tlist: Option<&NodeList<'_>>, ncols: usize, mcx: Mcx<'m>) -> PgVec<'m, bool> {
    let mut out = mcx::pg_vec![in mcx; false; ncols];
    sk.iter().filter(|k| k.compare_fn()).filter_map(|k| key_col(k, tlist, ncols)).for_each(|c| out[c as usize] = true);
    out
}

/// Whether key `i`'s tail makes a var run: an enum word (V4R-386), any tail from key 4 (V4R-548: a `SortSpec` holds four), or
/// a hosted key's (`proc`: its comparator and collation ride the var section, a `name` under a non-C collation).
pub(crate) fn var_key(i: usize, tail: u16, proc: u32) -> bool { tail >> 8 & 0x7f == WORD_WIDTH || i >= 4 && tail != 0 || proc != 0 }

/// Whether a varlena column orders by its type's comparator (rep.hosted.order: a numeric no packed lane holds, an inet / cidr,
/// an array, a range, a record, a user type): its key's tail is its var copy, compared by `cmp_proc`'s function; or an
/// enum word (rep.hosted.order.fixed, V4R-386), whose tail is its datum word (`WORD_WIDTH`).
pub(crate) fn hosted(tf: &TypeFacts) -> bool {
    tf.byval && tf.image.is_enum() || tf.typlen == -1 && rep::arm_kind(Rep::VarlenaPtr, rep::Accessor::Order, tf) == KindId::RepHostedOrder || no_image(tf)
}

/// USEROP: a fixed-width type with no order image (box, point, circle, lseg, a user fixed-width or by-value type: the
/// type table's `Canonical`), which only a user opclass orders (a DEFAULT one too): its key orders by that family's
/// comparator (`cmp_proc`) over its datum word or its image's address, its prefix word the constant 0 (`pairs`), never by
/// rep.hosted.order.fixed's refusal.
pub(crate) fn no_image(tf: &TypeFacts) -> bool { tf.typlen > 0 && tf.image.is_canonical() }

const _: () = assert!(WORD_WIDTH < 128 && WORD_WIDTH != VAR_WIDTH); // a tail's width is its 7 bits (`off / 8 | width << 8 | desc << 15`)

/// D3931: per run column, whether its key keeps a narrow pgrc packed numeric lane's exact word with no tail (`tails`, HO
/// condition (d)): the one decision a run's writers and its reader all read. One writer (a Sort's fill): the column's
/// decompose output is such a lane. A MergeAppend's run, which every arm writes from its own row (ops/append/merge.rs
/// `writer_arms`): every arm's cell there is such a lane at one scale (cell.rs `row_col_rep`, the cell the arm builds).
/// An arm that hands a varlena (a drained Sort's run, whose target list decompose files over the fill's row; a SetOp's
/// emit; a heap arm) or a lane at another scale keys the column by its var tail in every arm and in the reader, and
/// `pairs` takes D2974's `num_order_word` from either cell: one image for the merge, never a packed key over a varlena
/// cell ('a word order image that is not one exact word') or two scales' mantissa words compared as one.
pub(crate) fn packed_words<'g>(gx: &crate::graph::Gx<'g, '_>, cols: &NodeList<'_>) -> PgVec<'g, bool> {
    let lane = |r: Option<Rep>| match r {
        Some(Rep::PackedNum { facts, .. }) if facts.narrow() => Some(facts.scale),
        _ => None,
    };
    let arms = crate::ops::append::merge::writer_arms(gx, cols);
    (0..cols.len()).map(|i| {
        let own = lane(cols.iter().nth(i).and_then(|n| n.as_target_entry()).and_then(|te| gx.out_of(te.expr)).map(|o| o.rep));
        let col = i16::try_from(i + 1).unwrap_or(i16::MAX);
        let mut scales = arms.iter().map(|&(a, at)| lane(crate::cell::row_col_rep(gx, a, at, col)));
        match arms.is_empty() {
            true => own.is_some(),
            false => scales.next().flatten().is_some_and(|s| scales.all(|x| x == Some(s))),
        }
    }).collect_in(gx.mcx)
}

/// Each run column's key tail `(offset, width)` (runs/sort.rs `Shape::plan_coded`): a name's image, a uuid's, an interval's
/// big-endian pair (M6-TYPES), a bytes-ordered or `hosted` varlena's copy (not a packed lane's numeric, HO condition (d)),
/// an enum's datum word at `datums` (`WORD_WIDTH`); else none. USEROP: a column a CompareFn key orders (`cmp`, `cmp_cols`)
/// takes its comparator's argument whatever its type (`keysem::call`'s datum): its datum word (`WORD_WIDTH`), its
/// fixed image as it is stored (its width), or its varlena copy (`VAR_WIDTH`); a packed lane's numeric or a code column
/// keeps none (keys refuses the key by D1346's name).
pub(crate) fn tails<'m>(cols: &[Col], packed: &[bool], datums: u32, cmp: &[bool], mcx: Mcx<'m>) -> PgVec<'m, Option<(u32, u16)>> {
    cols.iter().enumerate().map(|(i, c)| match (c.tf.base, c.form) {
        _ if cmp.get(i).copied().unwrap_or(false) => match c.form {
            Form::Word => Some((datums + 8 * i as u32, WORD_WIDTH)),
            Form::Image { off } if (c.tf.typlen as u16) < WORD_WIDTH => Some((off, c.tf.typlen as u16)),
            Form::Var { off } if !packed.get(i).copied().unwrap_or(false) => Some((off, VAR_WIDTH)),
            _ => None,
        },
        (NAME, Form::Image { off }) => Some((off, NAME_BYTES)),
        (_, Form::Image { off }) if c.tf.image.fixed_norm() == Some(FixedNorm::Raw) => Some((off, c.tf.typlen as u16)),
        (_, Form::Image { off }) if ivl(&c.tf) => Some((off + 16, 16)),
        (_, Form::Image { off }) if no_image(&c.tf) && (c.tf.typlen as u16) < WORD_WIDTH => Some((off, c.tf.typlen as u16)),
        (_, Form::Var { off }) if c.tf.image.is_bytes() || hosted(&c.tf) && !packed.get(i).copied().unwrap_or(false) => Some((off, VAR_WIDTH)),
        (_, Form::Word) if hosted(&c.tf) => Some((datums + 8 * i as u32, WORD_WIDTH)),
        _ => None,
    }).collect_in(mcx)
}

/// A key's comparator: 0 unless its column is `hosted`; then the btree support function 1 of its sort operator's opfamily
/// (the catalog's), else, for a built-in ordering operator (`catalog::opdir::builtin_dir`, the bootstrap catalog's) where the catalog has no answer, the type's own
/// (`TypeFacts::cmp`, its default opclass's); none refuses by shape. And whether it may be the user's (the home's `user_cmp`), and
/// whether it is a builtin's total order (`total_cmp`).
pub(crate) fn cmp_proc<'m>(catalog: &dyn gravity_expr::Catalog<'m>, node: i32, sk: &SortKey, tf: &TypeFacts) -> (u32, bool, bool) {
    if !hosted(tf) && !coll_name(tf, sk.coll) && !sk.compare_fn() {
        return (0, false, false);
    }
    (native_cmp(catalog, node, ordering_proc(catalog, node, sk, tf), tf.typid), crate::effects::visible::rep::user_cmp(catalog, sk, tf), total_cmp(catalog, sk, tf))
}

/// A key of a run PostgreSQL's qsort orders in arrival order (ops/sort/exact_feed.rs): its comparator whatever its
/// type, called on the pairs that qsort calls it on (`Key::user`, KFLAG_USER: kernels `sort_exact`), never std's sort.
pub(crate) fn exact_proc<'m>(catalog: &dyn gravity_expr::Catalog<'m>, node: i32, sk: &SortKey, tf: &TypeFacts) -> (u32, bool, bool) {
    (native_cmp(catalog, node, ordering_proc(catalog, node, sk, tf), tf.typid), true, false)
}

/// The btree support function 1 of `sk`'s sort operator's opfamily (the catalog's), else, for a built-in ordering operator
/// where the catalog has no answer, the type's own; none refuses by shape.
fn ordering_proc<'m>(catalog: &dyn gravity_expr::Catalog<'m>, node: i32, sk: &SortKey, tf: &TypeFacts) -> u32 {
    let c_level = |p: u32| matches!(catalog.func(p).ok().and_then(|f| f.prolang), Some(LANG_INTERNAL | LANG_C));
    match catalog.ordering_cmp_proc(sk.opno) {
        Ok(p) if p != 0 => p,
        _ if tf.cmp != 0 && gravity_decompose::catalog::opdir::builtin_dir(sk.opno).is_some() => tf.cmp,
        // W8-EQUIVCLASS-SORTOP (D2206): a key PG cannot begin is never compared where served (the Sort raises at its first
        // call, ops/sort/sort_noproc.rs); its type's C-level comparator stands in for a run nothing reads
        _ if tf.cmp != 0 && sk.no_proc.is_some() && c_level(tf.cmp) => tf.cmp,
        _ => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a comparator-ordered sort key whose operator names no btree support function") }),
    }
}

/// TOTAL-ORDER (`Key::total`, kernels `KFLAG_TOTAL`): whether a hosted key's comparator is built-in code that reaches no
/// user code, so its order is total (builtin btree comparators are) and std's sort may order it: a key of its type's
/// default family (not a CompareFn key), over a type with an order image, whose default btree comparator is built in with
/// every component a DISPATCH one reads through the type cache (the effects home's `cmp_free`: btarraycmp's element,
/// btrecordcmp's named columns, range_cmp's subtype class; never an anonymous record's). numeric_cmp, bttextcmp /
/// btnamecmp under a collation, enum_cmp, network_cmp, bitcmp and an array's or a range's over built-in types are. Any
/// other hosted key may meet an order that is not total (a user comparator, or one a record's / an array's reaches) and
/// orders by PostgreSQL's qsort.
fn total_cmp<'m>(catalog: &dyn gravity_expr::Catalog<'m>, sk: &SortKey, tf: &TypeFacts) -> bool {
    !sk.compare_fn() && !no_image(tf) && crate::effects::cmp_free(catalog, tf.typid)
}

/// A `name` key (or a domain over one) under a collation that does not order by bytes: PostgreSQL's `btnamecmp` is then
/// `varstr_cmp` of the strings under the collation, so its 64-byte image tail is compared by that support function
/// (rep.hosted.order over the image's address, kernels `tail_pair`), its prefix word 0 (`pairs`).
pub(crate) fn coll_name(tf: &TypeFacts, coll: KeyColl) -> bool { tf.base == NAME && !coll.bytes_order }

/// A comparator that runs in pool workers, checked and resolved at lowering (`gravity_fn::resolved::native`): its
/// `pg_proc.prolang` is internal or C (uc1, 09-28: a LANGUAGE sql opclass raised "cannot execute SQL without an outer
/// snapshot or portal" at W8), and the key's word is its resolution's id, so a worker binds a copy with no catalog
/// access (main 09-28). Any other comparator refuses by the Shape `native` names.
/// A comparator a pool worker cannot call (`gravity_fn::resolved::pool_safe`: all but a scalar builtin's, over `typid`)
/// sends the pipeline to the leader (frame/mechs.rs), served serial (main 09-28, wp1).
pub(crate) fn native_cmp<'m>(catalog: &dyn gravity_expr::Catalog<'m>, node: i32, p: u32, typid: u32) -> u32 {
    match gravity_fn::typesem::keysem::rep::sort_cmp_site(catalog.func(p).ok().and_then(|f| f.prolang), p, typid) {
        Ok(id) => id,
        Err(why) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
    }
}

/// pg_proc.prolang of a C-level function: internal (12) or C (13).
const LANG_INTERNAL: u32 = 12;
const LANG_C: u32 = 13;

/// USEROP: whether ordering operator `opno`'s btree comparator (its family's support function 1) is C or internal: it runs
/// no SQL, so a sort whose rows arrive in an order of their own (a breaker's rows, a merge's arms) calls it on pairs PG's
/// need not meet without a statement of its own. One in SQL or a procedural language may raise on a pair PostgreSQL's sort
/// never meets (its message names the pair), so such a sort (ops/sort lower.rs) or merge (ops/append merge.rs) refuses by
/// name (`USER_ARRIVAL`).
/// D3082: or a LANGUAGE sql comparator whose body, as PostgreSQL's planner would inline it, raises on no input
/// (`effects::quiet_call` over `Facts::sql_inlined`: `CREATE FUNCTION rev_cmp(int8, int8) .. LANGUAGE sql AS
/// 'SELECT btint8cmp($2, $1)'`), which meets any pair without a raise. D3713: or its body as fmgr_sql runs it, past a
/// SECURITY DEFINER or a SET clause that names no search_path (`effects::quiet_pairs`): the wrapper's own raise does
/// not depend on the pair.
pub(crate) fn c_level_cmp(gx: &crate::graph::Gx<'_, '_>, opno: u32) -> bool {
    gx.facts.key(gravity_decompose::catalog::KeyReq::sort(opno)).is_some_and(|o| {
        o.cmp != 0 && (matches!(gx.catalog.func(o.cmp).ok().and_then(|f| f.prolang), Some(LANG_INTERNAL | LANG_C)) || gravity_decompose::effects::quiet_pairs(gx.facts, o.cmp))
    })
}

/// USEROP: the Shape of a sort keyed by another family's comparator in SQL or a procedural language whose rows do not
/// arrive in PostgreSQL's order (`c_level_cmp`).
pub(crate) const USER_ARRIVAL: &str = "a sort over another breaker's rows or a merge's arms keyed by another family's comparator in SQL or a procedural language (its rows arrive in an order PostgreSQL's sort does not see, so a raise would name another pair)";

/// USEROP: the Shape of a sort keyed by another family's comparator in SQL or a procedural language (`c_level_cmp`) over
/// a hash join in its own pipeline (its probe, or a right / full join's drain). PostgreSQL's hash join, once it runs in
/// batches (its inner past work_mem x hash_mem_multiplier, as planned or grown at run time), returns batch 0's rows, then
/// each later batch's: an order this join does not keep, so the comparator meets other pairs, and one that raises on a
/// pair raised where PostgreSQL returned rows, or returned rows where PostgreSQL raised (sort lane probe zz_sort_hj_raise
/// 3, 10-04: 49 of 216 raising pairs differed at work_mem 1MB, W0 and W8; none at 64MB, one batch). `USER_ARRIVAL`'s
/// rule for a breaker's rows, applied to a join whose order PostgreSQL's batches decide. Served where PostgreSQL certainly
/// holds the table in one batch and each outer row meets at most one inner row, or rows no sort key tells apart
/// (ops/shared/one_batch.rs).
pub(crate) const USER_BATCHES: &str = "a sort keyed by another family's comparator in SQL or a procedural language over a hash join in its pipeline (PostgreSQL's hash join in batches returns its rows in another order, so a raise would name another pair)";

/// M6-TYPES: an interval column; its run image is followed by a 16-byte key tail, rep.fixed.hash's `[days, usecs]` pair
/// (`interval_cmp_value`'s span, signed word by word) stored big-endian with the sign bit flipped: memcmp is its order.
pub(crate) fn ivl(tf: &TypeFacts) -> bool { tf.image.fixed_norm() == Some(FixedNorm::Interval) }

/// Column `c`'s interval tail words (0 for a NULL: its flags word orders it first).
pub(crate) fn ivl_tail(cx: &mut Lx<'_, '_, '_>, node: i32, row: &RowVals, c: usize, tf: &TypeFacts) -> [Value; 2] {
    let Some(cr) = row.col(c as i16 + 1) else { crate::refuse!(node, Reason::Bug { payload: "a sorted column past the row" }) };
    let w = (rep::arm::<HashArm>(&*cx, KindId::RepFixedHash).image)(cx, cr, tf, Scope::Program).w;
    let top = cx.b.const_raw(Type::I64, 1 << 63);
    w.map(|x| {
        let x = x.unwrap_or_else(|| crate::refuse!(node, Reason::Bug { payload: "an interval image of one word" }));
        let x = cx.b.int_bin(IntBinOp::Xor, x, top);
        cx.b.int_un(IntUnOp::Bswap, x)
    })
}

/// A comparator-ordered key's prefix word, the kernels host `host` (`name`) over its payload (0 on a NULL row, which `fold`
/// places): inexact, its var tail (the comparator) decides a tie, so most compares never call it. A numeric's is
/// `typesem::numeric::order_word` (kernels `NumOrderWord`, `numeric_cmp`'s order); an inet / cidr's `typesem::prefix::net_word`
/// (`NetOrderWord`, PostgreSQL's network abbreviation, `network_cmp`'s order; setop_core-0221); a bit / varbit's
/// `typesem::prefix::bits_word` (`BitsOrderWord`, `bit_cmp`'s order; setop_core-0124); a dictionary-coded bpchar's
/// `typesem::prefix::bytes_word` over its trimmed render (`BytesOrderWord`; sort_pgrc-0002, the dict bpchar tops).
fn payload_word(cx: &mut Lx<'_, '_, '_>, c: rep::CellRef<'_>, tf: &TypeFacts, name: &'static str, host: usize) -> Value {
    let (p, null) = rep::datum(cx, c, tf);
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
    cx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
    cx.b.switch_to(l_val);
    let p = crate::rep::varlena::hash::ptr(cx.b, p);
    let (bytes, len, mark) = crate::rep::varlena::hash::payload(cx, p, tf);
    let ctx = cx.frame().ctx;
    let flags = HostFlags { touches_memory: true, pure: true, ..HostFlags::default() };
    let h = cx.b.declare_host_once(name, &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), host, flags);
    let w = cx.b.call_host(h, &[ctx, bytes, len], Some(Type::I64)).expect("an order word host returns its word");
    // D2239: a detoasted copy goes back to the Call's scratch once the word is taken
    crate::rep::varlena::hash::release(cx, mark);
    cx.b.br(l_out, &[w]);
    cx.b.switch_to(l_out);
    cx.b.block_params(l_out)[0]
}

/// Whether key `key`'s word (over its key type facts `tf`, `key_tf`) is a bit / varbit's `typesem::prefix::bits_word`, which
/// marks itself whole by its low bit (up to 56 bits): the table flags it `KFLAG_WORDEXACT` (face/runs.rs), so an equal
/// whole word ties with no payload read and no call. `pairs` takes bits_word for exactly these keys.
pub(crate) fn word_exact_marked(tf: &TypeFacts, key: &Key) -> bool {
    key.dflt && key.tail >> 8 & 0x7f == VAR_WIDTH && tf.image.is_bits()
}

/// The `[flags, word]` pair of each key over `row` (`tfs`: the run columns' type facts). A key whose order is not a word's
/// or a C-ordered name's refuses by its order kind's name (rep.varlena.order, rep.packed.order, rep.fixed.order, ...). A
/// key with a tail (a var copy, M6 L0-a; a uuid's image or an interval's pair, M6-TYPES) takes its arm's prefix word,
/// inexact: the tail decides its ties.
pub(crate) fn pairs<'m>(cx: &mut Lx<'_, '_, '_>, node: i32, row: &RowVals, keys: &[Key], tfs: &[TypeFacts], mcx: Mcx<'m>) -> PgVec<'m, [Value; 2]> {
    let mut out = mcx::reserved_in(keys.len(), mcx);
    for key in keys.iter() {
        let Some(cr) = row.col(key.col as i16 + 1) else { crate::refuse!(node, Reason::Bug { payload: "a sort key past the row" }) };
        let tf = &key_tf(&tfs[key.col as usize], key);
        let var = key.tail >> 8 & 0x7f == VAR_WIDTH;
        let img = match cr.cell.rep {
            // a full sort orders a code by its rendered text across parts (its var tail), never by an in-part rank (V4R-118)
            Rep::Code { .. } if !var => crate::not_built!(node, Variant::Kind(KindId::RepVarlenaOrder)),
            // a bpchar code under a bytes-ordered collation: its trimmed render's first 8 bytes (rep.code.order's prefix keeps
            // the blanks, which bpchar does not compare), its tail trims (`KFLAG_BPCHAR`); under another collation it refuses
            // by rep.varlena.order's name below, as before
            // (not a comparator of a family of its own: a user opclass's order need not be the bytes')
            Rep::Code { .. } if tf.image.is_bpchar() && key.coll.bytes_order && (key.proc == 0 || key.dflt) => {
                crate::rep::code::datum::not_image_only(cx, cr.cell, "an image-only key cell read as an order image");
                payload_word(cx, cr, tf, "bytes_order_word", gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::BytesOrderWord> as *const () as usize)
            }
            Rep::Code { .. } if tf.image.is_bpchar() => crate::not_built!(node, Variant::Kind(KindId::RepVarlenaOrder)),
            // USEROP: a CompareFn key (a non-default family's comparator, `Key::dflt` false), a key of a type with no order
            // image (`no_image`) or any key whose comparator may be the user's (`Key::user`) orders by its comparator alone:
            // the constant word, inexact, so its tail decides every pair (the pairs PostgreSQL calls it on); never its type's
            // image, whose order need not be the family's
            _ if key.proc != 0 && (!key.dflt || no_image(tf) || key.user) => cx.b.iconst(Type::I64, 0),
            Rep::FixedPtr { len: NAME_BYTES } if tf.base == NAME && key.proc != 0 && coll_name(tf, key.coll) => cx.b.iconst(Type::I64, 0), // its comparator decides (types-0015)
            Rep::FixedPtr { len: NAME_BYTES } if tf.base == NAME => name_image(cx, cr, tf, key.coll)[0], // D513: a domain over name too
            // D2974: a packed lane's numeric (pgrc) takes the same prefix word as a varlena's, from its rendered datum: one
            // run is fed by every arm of an Append, and a heap arm's varlena and a pgrc arm's packed lane must give one
            // value one word (its mantissa word ordered the pgrc rows apart from the heap rows: 99.9 sorted above 100.9)
            Rep::VarlenaPtr | Rep::PackedNum { .. } if key.dflt && var && tf.image.is_num_form() => {
                payload_word(cx, cr, tf, "num_order_word", gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::NumOrderWord> as *const () as usize)
            }
            Rep::VarlenaPtr if key.dflt && var && matches!(tf.base, INET | CIDR) => {
                payload_word(cx, cr, tf, "net_order_word", gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::NetOrderWord> as *const () as usize)
            }
            Rep::VarlenaPtr if word_exact_marked(tf, key) => {
                payload_word(cx, cr, tf, "bits_order_word", gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::BitsOrderWord> as *const () as usize)
            }
            // the table marks this key's word whole by its low bit (face/runs.rs `KFLAG_WORDEXACT`): only bits_word's may be
            _ if word_exact_marked(tf, key) => crate::refuse!(node, Reason::Bug { payload: "a bit string sort key off a varlena cell" }),
            _ => match rep::order_words(cx, cr, tf, key.coll) {
                rep::Words { w: [Some(w), None], exact } if exact || key.tail != 0 => w, // an inexact prefix: its tail decides ties
                _ => crate::refuse!(node, Reason::Bug { payload: "a word order image that is not one exact word" }),
            },
        };
        // D443: a key is NULL exactly when its column is
        let pair = match fold(cx.b, img, cr.null, false, key.desc, key.nulls_first) {
            Folded::Pair(p) => p,
            Folded::One(w) => {
                let top = cx.b.const_raw(Type::I64, 1 << 63);
                [cx.b.iconst(Type::I64, 1), cx.b.int_bin(IntBinOp::Xor, w, top)]
            }
        };
        out.push_in(pair);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
    use crate::frame::row::Carry;
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::IntCond;
    use gravity_program::{NumTypmod, Until};

    const C: KeyColl = KeyColl { oid: 950, bytes_eq: true, bytes_order: true };

    /// The run pair of a one-key sort over a word cell of `typid` holding datum `d` (`None`: NULL).
    fn pair(typid: u32, d: Option<u64>, desc: bool, nulls_first: bool) -> (u64, u64) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let w = crate::catch(9, || {
            let mut pb = ProgramBuilder::new();
            let fid = pb.declare_func("k", &[Type::I64, Type::I64], &[Type::I64, Type::I64]);
            let mut b = pb.define(fid);
            let (v, nw) = (b.params()[0], b.params()[1]);
            let (zero, one) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1));
            let null = b.icmp(IntCond::Ne, nw, zero);
            let tf = *row_of(typid).expect("type facts");
            let cell = Cell { rep: Rep::Word { bytes: 8 }, typid, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe };
            let row = RowVals { row: Row { cells: mcx::pg_vec![in rig.mcx(); cell] }, vals: mcx::pg_vec![in rig.mcx(); (v, Some(null))], cols: mcx::pg_vec![in rig.mcx(); Some(0)], carry: Carry::new(zero, rig.mcx()), next: one, at: zero };
            let out = {
                let mut cx = Lx::new(&mut b, 9, rig.mcx());
                pairs(&mut cx, 9, &row, &[Key { col: 0, desc, nulls_first, coll: C, tail: 0, bpchar: false, proc: 0, dflt: false, eq_tie: false, user: false, total: false }], &[tf], rig.mcx())
            };
            b.ret(&out[0]);
            let prog = pb.finish();
            Interp::new(&prog).run(fid, &[d.unwrap_or(0), d.is_none() as u64]).expect("runs")
        });
        let w = w.expect("a pair");
        (w[0], w[1])
    }

    /// D508 (the row walk's bound, exec v4/m3_sort.rs `run_bound`): an ASC integer value's pair is `[1, w]` under NULLS FIRST
    /// and LAST, and `(w ^ 1 << 63) as i64` is the value, sign-extended at the type's width; a DESC pair's word is not (the
    /// walk never arms on DESC).
    #[test]
    fn an_asc_integer_pair_round_trips_to_its_value() {
        let kinds: [(u32, u32, &[i64]); 6] = [
            (21, 16, &[-300, -1, 0, 5, i16::MIN as i64, i16::MAX as i64]),
            (23, 32, &[i32::MIN as i64, -1, 0, 9, i32::MAX as i64]),
            (1082, 32, &[-10, 0, 400]),
            (20, 64, &[i64::MIN, -7, 0, i64::MAX]),
            (1114, 64, &[i64::MIN, -1, 0, i64::MAX]),
            (1184, 64, &[i64::MIN, 5, i64::MAX]),
        ];
        for (typid, bits, vals) in kinds {
            for &v in vals {
                let low = if bits == 64 { v as u64 } else { v as u64 & ((1u64 << bits) - 1) };
                for nf in [false, true] {
                    let (flags, w) = pair(typid, Some(v as u64), false, nf);
                    assert_eq!((flags, (w ^ 1 << 63) as i64), (1, v), "type {typid} value {v} nulls_first {nf}");
                    assert_eq!((pair(typid, Some(low), false, nf).1 ^ 1 << 63) as i64, v, "type {typid} value {v}: sign-extended at its width");
                }
                assert_ne!((pair(typid, Some(v as u64), true, false).1 ^ 1 << 63) as i64, v, "type {typid} value {v}: DESC does not round-trip");
            }
        }
    }

    /// D443 per key kind: the pairs, compared as the run compares them (flags, then the word, unsigned), put the values in
    /// the type's order (reversed for DESC) and every NULL first or last as NULLS FIRST / LAST says, NULLs tied.
    #[test]
    fn the_null_rule_and_direction_order_as_postgres() {
        let f8 = |x: f64| x.to_bits();
        let kinds: [(u32, Vec<u64>); 6] = [
            (21, vec![-300i64 as u64, 0, 5]),
            (23, vec![i32::MIN as i64 as u64, -1i64 as u64, 0, 9]),
            (20, vec![i64::MIN as u64, -7i64 as u64, 0, i64::MAX as u64]),
            (701, vec![f8(f64::NEG_INFINITY), f8(-2.5), f8(0.0), f8(1e300), f8(f64::NAN)]),
            (1082, vec![-10i64 as u64, 0, 400]),
            (16, vec![0, 1]),
        ];
        for (typid, vals) in kinds.iter() {
            for desc in [false, true] {
                for nf in [false, true] {
                    let mut xs: Vec<Option<u64>> = core::iter::once(None).chain(vals.iter().copied().map(Some)).chain([None]).collect();
                    xs.sort_by_key(|&d| pair(*typid, d, desc, nf));
                    let mut want: Vec<Option<u64>> = vals.iter().copied().map(Some).collect();
                    if desc {
                        want.reverse();
                    }
                    let nulls = [None, None];
                    let want: Vec<Option<u64>> = if nf { nulls.iter().chain(want.iter()).copied().collect() } else { want.iter().chain(nulls.iter()).copied().collect() };
                    assert_eq!(xs, want, "type {typid} desc {desc} nulls_first {nf}");
                    assert_eq!(pair(*typid, None, desc, nf), pair(*typid, None, desc, nf), "NULLs tie");
                    assert_eq!(pair(*typid, None, desc, nf).1, 0, "a NULL's word is 0");
                }
            }
        }
    }

    #[test]
    fn a_code_key_refuses_by_varlena_order() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let r = crate::catch(9, || {
            let mut pb = ProgramBuilder::new();
            let fid = pb.declare_func("k", &[Type::I64], &[Type::I64]);
            let mut b = pb.define(fid);
            let (v, zero) = (b.params()[0], b.iconst(Type::I64, 0));
            let tf = *row_of(25).expect("text facts");
            let cell = Cell { rep: Rep::Code { dict: gravity_program::DictRef::lane(0) }, typid: 25, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Never };
            let row = RowVals { row: Row { cells: mcx::pg_vec![in rig.mcx(); cell] }, vals: mcx::pg_vec![in rig.mcx(); (v, None)], cols: mcx::pg_vec![in rig.mcx(); Some(0)], carry: Carry::new(zero, rig.mcx()), next: zero, at: zero };
            let mut cx = Lx::new(&mut b, 9, rig.mcx());
            pairs(&mut cx, 9, &row, &[Key { col: 0, desc: false, nulls_first: false, coll: C, tail: 0, bpchar: false, proc: 0, dflt: false, eq_tie: false, user: false, total: false }], &[tf], rig.mcx());
        });
        let e = r.expect_err("a code key refuses");
        assert!(format!("{e:?}").contains("RepVarlenaOrder"), "{e:?}");
    }

    /// M6 L0-a: a var column's key carries a `VAR_WIDTH` tail entry, in the `SortSpec` at keys 0-3 and on the key alone past
    /// them (the var table's, V4R-265); an int key keeps its one word (G4); a tail past sixteen keys rides its key too (the
    /// comparator reads it from the var table, W8-SETOP-WIDEKEYS). V4R-548: a fixed image's tail (a name / uuid / interval)
    /// past four rides its key the same way, and makes its run a var run (`var_key`); at keys 0-3, or with no tail, it does not.
    #[test]
    fn a_var_key_carries_its_tail_entry() {
        use gravity_decompose::ops::shared::sort::KeyImage;
        use gravity_abi::breaker::KeyKind;
        let sk = |resno: i16, desc: bool| SortKey { resno, opno: 0, desc, nulls_first: false, kind: KeyKind::I64, image: KeyImage::Raw, scale: 0, coll: C, own_family: false, no_proc: None, eq_tie: false, per_row: false };
        let tails = [None, Some((96, VAR_WIDTH))];
        let (ks, spec) = keys(9, &[sk(1, false), sk(2, true)], None, &tails, &|_, _| (0, false, false));
        let t = 12 | VAR_WIDTH << 8 | 1 << 15;
        assert_eq!((ks[0].tail, ks[1].tail, spec), (0, t, (t as u64) << 16), "an int key has no tail (G4); the text key's entry");
        let five: Vec<SortKey> = [1, 1, 1, 1, 2].iter().map(|&r| sk(r, false)).collect();
        let (ks, spec) = keys(9, &five, None, &tails, &|_, _| (0, false, false));
        assert_eq!((ks[4].tail, spec), (12 | VAR_WIDTH << 8, 0), "a fifth key's entry rides the key, not the SortSpec");
        for w in [NAME_BYTES, 16] {
            let (img, t) = ([None, Some((96, w))], 12 | w << 8);
            let (ks, spec) = crate::catch(9, || keys(9, &five, None, &img, &|_, _| (0, false, false))).expect("a fifth image tail serves");
            assert_eq!((ks[4].tail, spec), (t, 0), "a fifth key's image entry rides the key (width {w})");
            let both: Vec<SortKey> = [2, 1, 1, 1, 2].iter().map(|&r| sk(r, r == 2 && w == 16)).collect();
            let (ks, spec) = keys(9, &both, None, &img, &|_, _| (0, false, false));
            let d = if w == 16 { 1 << 15 } else { 0 };
            assert_eq!((ks[0].tail, ks[4].tail, spec), (t | d, t | d, (t | d) as u64), "keys 0 and 4 over one image column");
            assert!(var_key(4, t, 0) && var_key(4, t | 1 << 15, 0) && var_key(16, t, 0) && var_key(126, t, 0), "a tail from key 4: a var run");
            assert!((0..4).all(|i| !var_key(i, t, 0) && !var_key(i, t | 1 << 15, 0)), "a tail at keys 0-3 rides the SortSpec (G4)");
        }
        assert!(!var_key(4, 0, 0) && !var_key(16, 0, 0) && !var_key(0, 0, 0) && var_key(0, 12 | NAME_BYTES << 8, 359), "a key with no tail is decided by its word (G4)");
        assert!(var_key(0, 12 | WORD_WIDTH << 8, 0) && var_key(5, 12 | WORD_WIDTH << 8, 0), "an enum word at any key (V4R-386)");
        let seventeen: Vec<SortKey> = (0..17).map(|i| sk(if i == 16 { 2 } else { 1 }, false)).collect();
        let (ks, spec) = crate::catch(9, || keys(9, &seventeen, None, &tails, &|_, _| (0, false, false))).expect("a seventeenth tail serves");
        assert_eq!((ks[16].tail, spec), (12 | VAR_WIDTH << 8, 0), "a seventeenth key's entry rides the key");
        // USEROP / D2041 lifted: past sixteen keys with a tail, a key whose comparator may reach user code (column 1's here)
        // serves at any position, its comparator riding its key; with none, or with no tail past sixteen, as before
        let (tails3, user) = ([None, Some((96, VAR_WIDTH)), Some((104, VAR_WIDTH))], |c: usize| if c == 1 { (7, true, false) } else { (0, false, false) });
        let run = |at: i32, last: i16| -> Vec<SortKey> { (0..18).map(|i| sk(if i == at { 2 } else if i == 17 { last } else { 1 }, false)).collect() };
        for at in [0, 16, 17] {
            let (ks, _) = crate::catch(9, || keys(9, &run(at, 3), None, &tails3, &|_, c| user(c))).expect("a user-reach key past sixteen serves");
            assert_eq!(ks[at as usize].proc, 7, "the user-reach key at {at} keeps its comparator");
        }
        let (ks, _) = crate::catch(9, || keys(9, &run(-1, 3), None, &tails3, &|_, c| user(c))).expect("no user-reach key serves");
        assert_eq!((ks[17].tail, ks[17].proc), (13 | VAR_WIDTH << 8, 0), "an eighteenth tail of no user reach rides its key");
        let near = &run(0, 1)[..17];
        let (ks, _) = crate::catch(9, || keys(9, near, None, &tails3, &|_, c| user(c))).expect("no tail past sixteen: trunk's form");
        assert_eq!((ks[0].proc, ks.len()), (7, 17), "the user-reach key keeps its comparator where trunk served it");
    }

    /// R2 13b: a key sorted by bpchar's operator over a text / varchar column takes bpchar's facts (its prefix and tail trim);
    /// by any other operator, or over a bpchar, name or bytea column, the column's own.
    #[test]
    fn a_bpchar_key_over_text_trims() {
        use gravity_abi::breaker::KeyKind;
        use gravity_decompose::ops::shared::sort::KeyImage;
        let key = |bpchar: bool| Key { col: 0, desc: false, nulls_first: false, coll: C, tail: 0, bpchar, proc: 0, dflt: false, eq_tie: false, user: false, total: false };
        let trims = |t: u32, bpchar: bool| key_tf(row_of(t).unwrap(), &key(bpchar)).image.is_bpchar();
        assert!(trims(25, true) && trims(1043, true) && trims(1042, true) && trims(1042, false), "bpchar's order trims");
        assert!(!trims(25, false) && !trims(1043, false) && !trims(19, true) && !trims(17, true), "any other key compares every byte");
        let sk = |opno: u32| SortKey { resno: 1, opno, desc: false, nulls_first: false, kind: KeyKind::Fp128, image: KeyImage::ByRef, scale: 0, coll: C, own_family: false, no_proc: None, eq_tie: false, per_row: false };
        let (ks, _) = keys(9, &[sk(1058), sk(1060), sk(664)], None, &[Some((96, VAR_WIDTH))], &|_, _| (0, false, false));
        assert_eq!(ks.iter().map(|k| k.bpchar).collect::<Vec<_>>(), [true, true, false], "bpcharlt / bpchargt, not textlt");
    }

    /// TOTAL-ORDER (b): a key is `total` (its table's `KFLAG_TOTAL`: std's sort) only with a comparator that `cmp_proc` calls
    /// a builtin's total order, never one that may be the user's, and never a key with no comparator.
    #[test]
    fn only_a_builtin_total_comparator_key_is_total() {
        use gravity_abi::breaker::KeyKind;
        use gravity_decompose::ops::shared::sort::KeyImage;
        let sk = SortKey { resno: 1, opno: 0, desc: false, nulls_first: false, kind: KeyKind::Fp128, image: KeyImage::ByRef, scale: 0, coll: C, own_family: false, no_proc: None, eq_tie: false, per_row: false };
        let total = |cmp: (u32, bool, bool)| keys(9, &[sk], None, &[Some((96, VAR_WIDTH))], &|_, _| cmp).0[0].total;
        assert!(total((7, false, true)), "a builtin total comparator");
        assert!(!total((7, true, true)) && !total((7, false, false)) && !total((0, false, true)), "a user's, one that may reach user code, none");
    }
}
