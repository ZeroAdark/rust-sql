//! Expression lowering's sizes (gravity_expr emit, lowerv4 step): each bounds the program a form unrolls, never an
//! answer (past each bound the general form serves the same value).

/// The longest IN / `= ANY` list (a constant array or a one-dimension `ARRAY[...]`) the scalar, vector and word forms
/// unroll element by element; a longer one is walked by the host, element by element through the site's cell. CHOSEN,
/// not measured: an unrolled compare per element keeps the program's size linear in a short list.
pub const MAX_LIST: usize = 16;

/// D2312's finite form (decompose ops/setop.rs `srf_ends`): the largest magnitude of a constant that bounds a recursive
/// union's target-list set-returning function (a series' start or stop, a LIMIT's count or offset) the lowering serves.
/// The union fills each generation whole where PostgreSQL streams it, so the series runs to its end before the first
/// row: at most 2^17 + 1 rows per input row. CHOSEN, not measured: a generation of that many rows fills in milliseconds.
pub const SRF_SMALL: u64 = 1 << 16;

/// The shortest constant text / varchar IN list (texteq `= ANY`, textne `<> ALL`, under a collation whose equality is byte
/// equality) expr.saop answers through its byte set (lowerv4 step/raw/saop_set.rs: one probe of a (length, head / tail
/// bytes) hash, then the bytes compared) instead of walking the elements: FITTED (lanes/expr DESIGN-OPEN LIST-FORM, dev
/// table, misses and hits, 4- to 1000-byte elements): the walk compares the length and then the bytes of each element,
/// the set costs a constant probe whatever the elements' count and width. R5-B: `forms::text_set_min` reads it from the
/// class table's rows (2 there too); this is the rule of before, kept only for a table without those rows.
pub const TEXT_SET_MIN: usize = 2;

/// The list length from which PostgreSQL's plan hashes an IN list (MIN_ARRAY_SIZE_FOR_HASHED_SAOP, optimizer/util/
/// clauses.c): a LIST-FORM byte set (lowerv4 step/raw/saop_bytes.rs) of at least this many elements, three of them alike
/// in their length, head and tail, keys every byte, as PG's hash reads them. PG's, not measured.
pub const TEXT_SET_FULL_MIN: usize = 9;

/// The shortest constant list under an ordering operator gravity_expr compares in line (a by-value operator it emits
/// itself, prim) that expr.saop answers through the list's extreme instead (lowerv4 step/raw/saop_extreme.rs: one host
/// call, one compare): FITTED (lanes/expr DESIGN-OPEN LIST-EXT, int4 `< ANY` / `>= ALL` over 2-16 elements, 200k rows: the
/// call is 2.7x the unrolled compares at 2 elements, 1.05 / 1.00x at 16, W0; 1.9x / 1.03x at W8), so a prim list the
/// scalar form unrolls keeps them. A list past `MAX_LIST`, and any list of two or more under an operator gravity_expr
/// calls, takes the extreme whatever its length. R5-B: `forms::extreme_prim_min` reads it from the class table's rows by
/// element class at both ends; this is the rule of before, kept only for a table without those rows.
pub const EXTREME_PRIM_MIN: usize = MAX_LIST + 1;

/// FieldStore: the (field number, value) pairs one host call stores (a longer store is several calls over the same
/// image). CHOSEN, not measured: it bounds one call's argument words.
pub const FIELD_STORE_BATCH: usize = 40;

/// The longest LIKE / memmem literal a text claim compares inline (unrolled word compares); a longer one is compared by
/// its first MAX_LIT bytes (a suffix's last) where the whole literal fits, PostgreSQL's call deciding the rows where they
/// occur. CHOSEN, not measured: it bounds the unrolled compares, never which literals are claimed.
pub const MAX_LIT: usize = 64;

/// The longest value (and ILIKE pattern) a folded LIKE claim (`lower(col) LIKE`, ILIKE) lowers into its frame scratch
/// before the test; a longer one takes PostgreSQL's call for that row (that execution). CHOSEN, not measured: it bounds the
/// claim's stack scratch, never which statements are claimed.
pub const MAX_FOLD: usize = 1024;
