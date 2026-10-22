# How to lower an alloclint row (for every lane whose change removes or moves a heap allocation in a Gravity crate)

The ledger `crates/gravity/lint/ALLOCLINT-DEBT/<crate>/<file>` holds a budget of untracked allocation sites per source file:
one row `kind<TAB>item<TAB>count` per detector item (`kind` = type | method | growth | macro from clippy, `ir-only` from the
IR cross-check). `<crate>` is the crate's dir under crates/gravity with '/' as '.' (lowerv4, backend.interp); `<file>` is the
file's path with '/' as '.', plus `.tsv`. (The ledger dir has no README since 2026-10-15: a generated file collided in merges, TD-203.
Its description is `LEDGER_DOC` in ratchet.py.) The rows only go DOWN, and they must equal what the code has now. So a change that removes a
site, or moves it into a home, lowers its row IN THE SAME CHANGE. The check fails both ways.

| leg | where | what FAILs it |
|---|---|---|
| `alloclint-base` (ledger only, < 1 s) | S0 quick on the Mac, S1, S2 | a row added or raised; a new `homes.tsv` row with no `Alloclint-Home:` trailer |
| per-crate clippy leg (`run.sh OUT clippy <crate dirs>`) | S1 dev + S2, only crates the range touches | `FAIL NEW` (a site above its row) and `FAIL STALE` (a row above the code) |
| IR leg (`run.sh OUT full`) | S3-slow | a new `ir-only` site (RED, FOR MAIN); a stale `ir-only` row is a note only |

## The usual case: your conversion removed sites (FAIL STALE)
The clippy leg prints, for example:
`alloclint FAIL STALE crates/gravity/lowerv4/src/ops/hashjoin/spill/raw/region.rs growth push: budget 2, now 1: lower the row`

Either way below is fine; both give the same file.
1. **By hand (you know what you removed).** Open
   `crates/gravity/lint/ALLOCLINT-DEBT/lowerv4/crates.gravity.lowerv4.src.ops.hashjoin.spill.raw.region.rs.tsv` and set
   `growth<TAB>push<TAB>1`. If a row reaches 0, delete the line. If only the `path` line is left, delete the file.
   (If the crate's directory ends up empty, leave an empty `.empty` file in it.)
2. **By the tool (on a box: cargo).** From the worktree:
   `crates/gravity/lint/alloclint/run.sh /work/<lane>/al clippy crates/gravity/<crate>`. It prints every NEW / STALE line.
   Then run `python3 crates/gravity/lint/alloclint/ratchet.py write --findings /work/<lane>/al/clippy.json --crate crates/gravity/<crate>`.
   It rewrites only that crate's directory and keeps its `ir-only` rows.

Then, on the Mac, run `python3 crates/gravity/lint/alloclint/ratchet.py base --base origin/malisper/gravity-v5`. It must say
`PASS: the ledger only shrinks (A -> B sites)`. Commit the ledger files in the same commit as the code (or the same range).
Name the lowered rows in the commit message (`alloclint: region.rs growth push 2 -> 1, method with_capacity 2 -> 1`).

## A new site (FAIL NEW): never raise the row
Allocate through a home instead of the global heap:
- an mcx context;
- a driver region / pool block via Charge;
- the program arena;
- a cache holder or cache scope;
- an operator arena (`OpArena` / `AVec` / `AMap` in crates/gravity/fn: `homes.tsv` H5).

The detector does not count a site inside a home. A NEW homes.tsv row (a new function that is a home) is a reviewed exemption:
the commit must carry the trailer `Alloclint-Home: <file> <function>`, or `alloclint-base` FAILs.
Moving a site to another file is a NEW in the new file plus a STALE in the old one. Use a home, or lower the old row and keep
the new one at its count only where a home is impossible (that needs main's word: it is a raise).

When no home fits, a new row needs the USER (main relays the ask). Once the user accepts it, write the row in the ledger and
name it in a commit trailer of the range, one trailer per row:
`Alloclint-Approved: <file> <kind> <item> <count> USER-DECISIONS-<file>.md@<ts>` (the ruling's file and timestamp).
`alloclint-base` then lets that row be added or raised by at most <count>. A trailer that cites no user decision FAILs.
The item may hold spaces (an IR item such as `<T as core::convert::Into<U>>::into`): write it as the ledger has it, no quotes.
The words are read from both ends: <file> and <kind> first, the citation last, <count> just before it, the item in between.
A lane never approves its own rows.

## Error, refusal and log text: tag it `// alloc: Cold` (COLD-TEXT, not debt)
USER RULING (via main 2026-10-14T18:52:43Z): a site whose value is text that leaves by unwinding or logging (refuse! /
Reason, PgError, eprintln!, report lines) is a NAMED class, COLD-TEXT. End the line with a plain `// alloc: Cold`. If the
detector's item is text by name (format!, String, to_string, push_str, str::*, CString / OsString), the site gets no row:
the check prints it in its own column (`COLD-TEXT n`), outside the debt total. So a removed row for such a line is lowered
as usual (STALE until you do).
- A Cold tag on a non-text allocation (a Vec, Box, map, Arc, ...) stays debt; the check counts it in its MISTAGGED column
  (gravity_lint's Cold lifetime on a cold path is fine as a tag, but it does not take the site out of the debt).
- A Cold tag on an item that is text only for some receivers (`to_owned`, `clone`, `join`, `collect`, `into`) stays debt
  (COLD-UNPROVEN). Write the text form (`.to_string()`, `String::from`, `format!`) to make it COLD-TEXT.
- An error payload is COLD-TEXT by its TYPE (TD-197, memacct 2026-10-15): a Cold-tagged `Box::new(e)` whose box the IR
  shows holds a `PgError` (the instance `Box::new<PgError>`, called or inlined) needs no turbofish. Such lines go to the
  crate's `.noalloc` with class COLD-TEXT, keyed on the line's text, and a full run re-proves them (an edited line is
  MISTAGGED again until it does). On ir-only rows, PgError's constructors (`PgError::new` / `error` / ..) are text items, so
  a Cold-tagged line building an error is COLD-TEXT (D4611, D4616).
- Only a plain Cold tag names the class. `// alloc: Exec` on a format! is debt; `// alloc: Cold charged <owner>` is a charge (HC).
- `// alloc: <L> charged caller` (D4505, USER GO 2026-10-15T05:52:10Z) is a charge (HC) only inside a pass-through writer
  that `crates/gravity/lint/alloclint/clippy.toml` lists as a disallowed path (`{ path = "<crate>::<mod>::<fn>", reason =
  "alloclint:growth" }`, e.g. pgrc_format's `wire::put_u32`): each CALLER's call site is then a finding and a row in the
  caller's file. Anywhere else the tag charges nothing. The line's findings are debt, counted MISTAGGED. To take such a site
  out of the debt, charge its bytes for real (a ChargedVec, or a `charged <owner>` that names the owner). Listing a new
  pass-through writer in clippy.toml moves its rows into the callers' files, which is a reviewed change (post FOR MAIN).

## A type named on a line that allocates nothing (TYPE-MENTION, not debt)
USER RULING (via main 2026-10-14T23:20:10Z): a `type` finding (a field type, a signature, a turbofish, a `use` of
`Vec` / `HashMap` / `Arc` / ...) on a line with NO method / growth / macro finding is a TYPE-MENTION. It has no row, is never
NEW, and is outside the debt total. The check prints the count right after the debt (`TYPE-MENTION n`), and a full write
records it in the crate's `.columns`. A field or signature type needs no rewrite and no tag.
- A type finding on a line that also allocates (`let v: Vec<u8> = Vec::with_capacity(n);`) stays debt: its `type` row stands
  beside the line's growth / method / macro rows. Convert the allocation and both rows go.
- The test reads the clippy findings line by line. A type on line 1 and its allocation on line 2 of one statement is a mention
  plus a debt row for the allocation.
- A mention covers nothing in the IR leg (main 2026-10-14T23:41:27Z): an allocation the IR sees in a statement whose only
  finding is a mention (`let v: Vec<u8> = make();`, the allocation inside make()) is an `ir-only` row. The range that brought
  TYPE-MENTION in moved such allocations from their type rows to ir-only rows (`Alloclint-Catchup: type-mention`, accepted by
  `ratchet.py base` once, per file only while the file's type + ir-only total does not rise). Every later range follows the
  usual law.

## An Rc / Arc / Weak clone alone on its line (REFCOUNT-CLONE, not debt)
USER GO (via main 2026-10-15T11:46:31Z): a `Clone::clone` finding whose RESOLVED receiver is an `Rc<T>` / `Arc<T>` /
`rc::Weak<T>` / `sync::Weak<T>`, on a line with no other finding, is a REFCOUNT-CLONE: a count bump, no allocation. It has no
row, is never NEW, is outside the debt total, and is printed beside TYPE-MENTION (`REFCOUNT-CLONE n`, `.columns`
refcount-clone). The type is the compiler's, not the name's: collect.sh forces clippy's `clone_on_ref_ptr`, and only a clone
inside that lint's span counts. No rewrite and no tag is needed.
- `(*rc).clone()` and `T::clone(&*rc)` clone the T: debt. `Arc::clone(&a)` (a path call the lint does not check): debt. Write
  `a.clone()` to make it a REFCOUNT-CLONE.
- A line with any other finding (the `Arc` type named beside it, a Vec clone, an `Arc::new`) keeps every finding as debt.
- `Rc::new`, `Arc::new`, `make_mut`, and clones of Vec / Box / String / maps stay debt rows.
- Like a mention, it covers nothing in the IR leg.

## A clone / to_owned / extend / collect the IR proves (NOALLOC, HOMED: not debt)
USER RULING via memacct 2026-10-15T20:3xZ: a false positive is fixed in the lint. clippy names the trait method, not the
receiver, so these items fire on copies that allocate nothing (a derive(Clone) of u32 / bool fields, to_owned on an i32) or
that allocate only into a home (an ArenaVec = Vec<_, ArenaRef> clone into the plan arena, a PgVec / ChargedVec extend). The
IR leg (`run.sh OUT full`) proves the statement makes no global-heap allocation and writes it to `x/noalloc.tsv`: class
NOALLOC (nothing allocated) or HOMED (only through a home: mcx::Mcx, the program arena's ArenaRef, an operator arena).
`ratchet.py write --noalloc OUT/x/noalloc.tsv` keeps them per crate in `ALLOCLINT-DEBT/<crate>/.noalloc`, keyed on the
line's normalized text; the clippy leg then counts them in the NOALLOC / HOMED columns, not as rows.
- Adding or changing `.noalloc` rows needs the commit trailer `Alloclint-Noalloc: <crate dir>` (base FAIL NOALLOC).
- An edited line is debt again (NEW) until a full run re-derives its row. The S3-slow alloclint-ir slot keys a ledger row
  the IR no longer proves (`NOALLOC-UNPROVEN`).
- A line the IR cannot see (BLIND: a derive(Clone) nobody calls, a generic never instantiated) stays debt. Drop the unused
  derive, or make the copy reachable code.

## A function no server path reaches (UNREACHED, not debt)
USER RULING via memacct 2026-10-15 (task A (3)), GO 20:3xZ: a function listed in `crates/gravity/lint/alloclint/unreached.tsv`
(`<file>\t<function>\t<note>`) whose name `unreached.py prove` finds only in its definition, comments, tests / examples /
benches / build.rs, `#[cfg(test)]` items, or packages outside the postgres binary's normal-dependency closure has its
findings in the UNREACHED column, not as rows. `ratchet.py base` re-proves every row on every range: a landing that reaches
the function by name (any call, any string) FAILs UNREACHED. Drop the row then, and its sites become debt (FAIL ADDS:
convert them). A common method name (push, compile, load, walk) cannot pass: convert instead.

## mcx-transmute rows: a lifetime-extended context handle (USER APPROVED via main 2026-10-15T00:10:23Z)
A handle (`Mcx`) is its context's address. Extending it (`transmute::<Mcx<'_>, Mcx<'static>>(ctx.mcx())`) is sound only
while the context never moves after the handle is taken. The box-24 SIGABRT
was a context held BY VALUE, moved into an Rc after its handle was taken. The ledger
`crates/gravity/lint/ALLOCLINT-DEBT/mcx-transmute/<file>.tsv` (rows `mcx-transmute<TAB>transmute<TAB>n`: one per transmute call naming an Mcx; every crate
outside crates/_support/mcx, backend included; `mcxt.py`) holds the census of 2026-10-15 as debt. `ratchet.py base` (S0,
S1, S2) judges every changed file: its sites must EQUAL its rows.
- A new site (FAIL MCX NEW): take the handle from an address-stable owner instead: `PinnedContext::handle` (put the
  by-value context into `PinnedContext::new` BEFORE any handle is taken), `McxOwned` / `SendRoot`, or a static / leaked
  context. A function built only that way may be a home: a homes.tsv row (H3) named in an `Alloclint-Home:` trailer.
- A removed site (FAIL MCX STALE): lower the row in the same change (delete it at 0; an emptied directory keeps `.empty`).
- `python3 crates/gravity/lint/alloclint/mcxt.py scan --repo . <file>` lists a file's sites (no cargo, any host).

## ir-only rows
These are sites only the IR sees (a generic instantiated in another crate, a derive, a closure). The S3 IR leg names a stale
one as a note. Lower it by hand like a clippy row, or let the integrator's next `run.sh OUT full` + `ratchet.py write
--findings OUT/clippy.json --ir-misses OUT/x/misses.tsv --crate ..` do it.

## A crate moves (a path rename: USER RULING 2026-10-15 "Carry trunk's allowances")
List it in the repo root's `comment-budget-renames.tsv` (`<new crate dir><TAB><old crate dir>`, the one map comment-ratchet.py
and alloc_ratchet.py read too) and rename its scope.tsv row, ledger directory and homes.tsv rows in the same change. `ratchet.py
base` then holds each row to the old path's row (the renamed crates' names inside an item, and a moved file's module stem,
spelled the old way): the same count passes, a higher one RAISES, a row with no old row ADDS. A moved crate is no crate new in
scope (no `Alloclint-Scope:` trailer, which opens nothing for it), and a moved home needs no `Alloclint-Home:` trailer.

## A crate joins scope (alloclint's job, not a conversion's)
When main adds a crate, or a file of a file-scoped crate, to `scope.tsv`, the same change seeds its rows. Run `run.sh OUT full`
on the tree with the new row, then `ratchet.py write` (it writes only the new crate's directory; the others stay at their
budget), then size its lines in `size_class.tsv`. A commit in the range carries the trailer `Alloclint-Scope: <crate dir>`.
`ratchet.py base` (S0) accepts the added rows of a file in scope at the tree and not at the base only when such a trailer names
its crate; a crate already in scope can never add. From the seed on, its rows only go down.
The seed must carry the crate's clippy kinds (type / method / growth / macro), not the IR leg's rows alone: collect.sh
re-lints a selected crate with no ledger directory (cargo replays an empty message list for a crate it last checked as a
plain dependency), `write` refuses a new crate's seed of IR-only rows, and `base` FAILs SEED on one (>= 20 sites). Before
the landing, run `run.sh OUT clippy <crate dir>`: every file must be at its budget (parquet_read, alloclint#3286).

## A same-site rename (main 2026-10-14T22:10:42Z)
When a change keeps an allocation where it is but the detector names it differently (an IR list that is no longer Clone:
`b.params.clone()` -> `b.params.to_vec()`), write the ledger as usual (the old item falls, the new one rises) and add ONE row
to `renames.tsv` in the same range: `file line old_kind old_item new_kind new_item count lane reason`, `line` = the tree's line.
`ratchet.py base` (S0) accepts the new item's rise only when the line is a changed line whose removed twin in the same change
block is the same line with the new item's name spelled as the old one's (or the line at the same place in a block that keeps
its line count), the old item falls by at least `count`, and the file's total debt does not rise. The S1 leg prints the row and
FAILs it when the new item has no finding on that line. A new site, or one moved to another function, is never a rename: home
it. If base cannot prove the site, post FOR MAIN.

## When two changes cross
If your range and another landing both lower the same file, take the lower count of the two. The rows must still equal the
code: run the clippy leg after the rebase. Wave-1 lanes post their lowered rows (file, kind item, old -> new) on their STATUS
line. The integrator (memlead) keeps the list, so a conversion that lands before the ratchet is lowered at the ratchet's rebase.

Why: reports/ALLOCLINT-GRAVITY-20261014.md (sections 5-7); the law is lint-alloc's (TD-114): budgets only shrink, and there is
no exception path.
