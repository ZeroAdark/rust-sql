"""RH (R-AHEAD; construct C2, licence-typed departures, reports/BUG-RECURRENCE-20261011.md §3 C2): a strategy that
schedules work before PostgreSQL 18.6's executor first demands it carries a `Licence`. Running ahead (an eager build, a
read-whole, a fill at open, an evaluation at Bind, a build before the first outer row) is a departure (decompose
effects/licence.rs `Departure::Ahead`); where the early work can be observed (it may raise, it acts) it is legal only
under a licence (`effects::licence` with a recheck, or `effects::licence_unchecked`, ruling (b): DECISIONS.md 4 (D) b,
tagged), and where it cannot be observed (no effects, or an error held to PostgreSQL's point) the strategy still holds
the licence the effects answer grants it, so the type records that the early work was asked about. Two bugs of the class (D3897: a nest's inner
Hash Right Join built before the nest's first outer row; D3817: a merge inner run before the merge reached it) ran
ahead with no licence. The licence home is decompose/src/effects/licence.rs: `Licence` has private fields, only the
home's functions make one, so a strategy that holds one cannot be built without the effects answer.

A run-ahead strategy is found three ways (text, over the strategy crates: decompose, lowerv4, lower and exec; the
runtime crates run what these decided):
- variant: an enum variant whose name says it runs ahead (a CamelCase word in `VARIANT_WORDS`: Eager, Ahead, Prefill,
  Upfront, Prebuild), or one `STRATEGIES` names (the census found it under another name, e.g. `SetForm::Ahead`'s
  siblings); it carries a licence when its payload names `Licence`;
- flag: a struct field whose name says it (a word in `FLAG_WORDS`, or a name in `FLAG_NAMES`: `whole_read`,
  `read_whole`, `at_open`, `at_bind`), or one `STRATEGIES` names; it carries one when its type names `Licence` or
  `AheadTag` (the run-ahead tag only `Px::ahead_unchecked` records, under `licence_unchecked`);
- fn: a function whose name says it decides to run ahead (a word in `FN_WORDS`), or one `STRATEGIES` names; it carries
  one when its signature names `Licence` (it takes the licence it schedules under, or returns the one it was granted).
`NOT_AHEAD` lists the names those words find that schedule nothing ahead, each with the reason (a review reads it).

The registry, lint/AHEAD-DEBT (a directory, one row per file, as LIFETIME-DEBT), lists each detected site that holds
no licence, by path and name (`Enum::Variant`, `Struct.field` or the fn's name), with its owner lane, its kind
(variant, flag, fn, bind) and what runs early. A detected site with no row fails. On a landing (--base) a row the parent
lacks fails (the list only shrinks), unless it replaces a row of the same name the landing removes (a certified move or
split). A row whose name its file no longer defines is a note, not a failure (delete it). The owner deletes a row when
the strategy holds a `Licence` or stops running ahead. Done when the registry is empty (CONSTRUCTS-20261011.md)."""
import re

from common import G, ledger_loc, Src, generated, in_any, walk_sources
from rl_lifetime import functions

ID = "RH"
TITLE = "R-AHEAD: a strategy that runs work before PostgreSQL's first demand holds no Licence"
INSTEAD = "hold the `Licence` the effects home grants (decompose effects::{licence, licence_unchecked}; lowerv4 " \
          "Px::{run_ahead, ahead_unchecked}) in the variant / flag / signature, or run at PostgreSQL's demand point; " \
          "not yet moved: a lint/AHEAD-DEBT row"
GLOBAL = True
DEBT = G + "lint/AHEAD-DEBT"
_HOME = [G + "decompose/src/effects/licence.rs"]
_CRATES = ("decompose", "lowerv4", "lower")
# exec: the bind-time half only (admission, the portal and the runner); exec/src/ops/** runs what lowering decided
_EXEC = (G + "exec/src/admit/", G + "exec/src/runner/", G + "exec/src/shapes.rs")
KINDS = {"variant", "flag", "fn", "bind"}

VARIANT_WORDS = {"Eager", "Ahead", "Prefill", "Upfront", "Prebuild"}
FLAG_WORDS = {"eager", "ahead", "prefill", "upfront", "prebuild"}
FLAG_NAMES = {"whole_read", "read_whole", "at_open", "at_bind"}
FN_WORDS = {"eager", "ahead", "prefill", "upfront", "prebuild"}

# Census-named strategies whose names do not say they run ahead: `Enum::Variant`, `Struct.field` or a fn name, by the
# file that defines them (lanes/licence/CENSUS-c2.tsv holds the reading of each).
STRATEGIES = {
    G + "decompose/src/facts/hoist.rs": {"plan"},
    G + "decompose/src/graph.rs": {"PipelineGraph.first_cte"},
    G + "decompose/src/ops/agg/mod.rs": {"held_whole"},
    G + "decompose/src/ops/limit.rs": {"window_keeps_stop"},
    G + "decompose/src/ops/scan/closure.rs": {"map_sample"},
    G + "decompose/src/ops/setop.rs": {"SetShape.right_first", "map_recursive_union"},
    G + "decompose/src/ops/setop/stream.rs": {"sorted_scan"},
    G + "decompose/src/ops/shared/setop.rs": {"SetForm::Grouped"},
    G + "decompose/src/self_spool.rs": {"raise_whole", "spool_whole", "store_whole", "wrap", "wrap_tree"},
    G + "exec/src/admit/entry/run.rs": {"open"},
    G + "exec/src/admit/mod.rs": {"eval", "graph"},
    G + "exec/src/runner/execute.rs": {"simple_at"},
    G + "exec/src/runner/plan.rs": {"late_words"},
    G + "exec/src/runner/portal.rs": {"bind_owned"},
    G + "lowerv4/src/effects.rs": {"walk_reversible"},
    G + "lowerv4/src/face/pull.rs": {"Form::Grows"},
    G + "lowerv4/src/frame/hoist.rs": {"late_roots"},
    G + "lowerv4/src/lifetime/material.rs": {"fill_form"},
    G + "lowerv4/src/ops/agg/trans/hosted.rs": {"keyed_slots", "rows_unchecked"},
    G + "lowerv4/src/ops/append/later_arms.rs": {"fenced"},
    G + "lowerv4/src/ops/append/merge.rs": {"make_end"},
    G + "lowerv4/src/ops/ctescan/lower.rs": {"Pull::Grow", "fills_parallel"},
    G + "lowerv4/src/ops/group/regroup.rs": {"call_order"},
    G + "lowerv4/src/ops/hashjoin/build_class.rs": {"defers", "raise_only"},
    G + "lowerv4/src/ops/hashjoin/drain/lower.rs": {"end"},
    G + "lowerv4/src/ops/hashjoin/probe.rs": {"Probe.lazy", "make", "make_swapped"},
    G + "lowerv4/src/ops/lockrows/rerun.rs": {"admit_lazy"},
    G + "lowerv4/src/ops/mergejoin/held_inner.rs": {"fill_holds"},
    G + "lowerv4/src/ops/mergejoin/lower.rs": {"make"},
    G + "lowerv4/src/ops/scan/pgrc/verdict_zone.rs": {"OpenParam::Init", "OpenParam::Stable"},
    G + "lowerv4/src/ops/shared/limit.rs": {"Lim::Word"},
    G + "lowerv4/src/ops/sort/inc_held.rs": {"producer"},
    G + "lowerv4/src/ops/subplan/ahead.rs": {"reader"},
    G + "lowerv4/src/ops/subplan/per_row.rs": {"certain_first"},
    G + "lowerv4/src/ops/window/lower.rs": {"admit"},
    G + "lowerv4/src/ops/window/rows.rs": {"Arg::Ev"},
    G + "lowerv4/src/rerun/inner_once.rs": {"kept_run"},
    G + "lowerv4/src/rerun/spool/form.rs": {"held"},
    G + "lowerv4/src/rerun/spool/lazy.rs": {"declare", "drain_wait"},
    G + "lowerv4/src/rerun/store_read.rs": {"fed"},
    G + "lowerv4/src/rerun/union_arms.rs": {"admit", "filled_once", "refill_arms", "source"},
    G + "lowerv4/src/step/expr_saop.rs": {"Arr::Set"},
}

# Names the words find that schedule nothing ahead of PostgreSQL's demand (file, name): the reason.
NOT_AHEAD = {
    (G + "decompose/src/graph.rs", "eager_initplans"): "eager_mask over this graph's planner costs (graph.rs:837), read only by lower inputs/key.rs cost_decide, whose one reader narrows pgrc's OpenParam::Init set (lowerv4 ops/scan/pgrc/lower.rs:279); guard: USES (eager_initplans, cost_decide)",
    (G + "decompose/src/graph.rs", "eager_mask"): "a cost pick (graph.rs:748): its mask only drops OpenParam::Init members from a pgrc scan's open (lowerv4 ops/scan/pgrc/lower.rs:279-287); what runs at the open is OpenParam::Init's, its own AHEAD-DEBT row; guard: USES (eager_mask only in graph.rs and lower inputs/key.rs)",
    (G + "decompose/src/ops/agg/gsets.rs", "fold_ahead"): "the raising fold asks licence() and records the grant (Builder.ahead_licensed); a denial for the group's own acting code still folds ahead, which changes call counts only: the rows keep PostgreSQL's values and extra VOLATILE calls are free",
    (G + "decompose/src/ops/limit.rs", "ahead"): "the guard: refuses a breaker under a Limit unless its node holds a licence (Builder.ahead_licensed)",
    (G + "decompose/src/ops/shared/breaker.rs", "spool_run"): "a graph shape (breaker.rs:68): seals a merge join's inner as one run or spool; whether it runs before the outer is lowerv4 ops/mergejoin/lower.rs make's choice (run_ahead at 209, ahead_unchecked at 228); guard: USES (spool_run only from decompose join.rs:543)",
    (G + "decompose/src/ops/shared/setop.rs", "IterDesc.read_whole"): "a fact: every reader reads the union to its end, so PostgreSQL's demand reads the seed whole too",
    (G + "decompose/src/ops/shared/setop.rs", "SetForm::Ahead"): "a form decompose picks; lowerv4 setop/lower.rs make holds run_ahead's licence in Copies.ahead or refuses (SORTED_SPOOL)",
    (G + "decompose/src/ops/subplan/mod.rs", "Read::All"): "a fact (mod.rs:966): how far PostgreSQL reads a node's rows; the window SubPlan fill it lets run is attach_window_args's (mod.rs:905, its licence asked at 951); guard: Read is private to its module",
    (G + "decompose/src/ops/subplan/mod.rs", "read_on_demand"): "a store-form choice (map_init_plans): the CTE's own Materialize, pulled at its readers' demand (lowerv4 ops/ctescan, m8_cte_pull), or the spool shortcut onto the CTE plan's root breaker; it runs nothing itself: the shortcut's producers are the CTE plan's own pipelines, and a run of one before PostgreSQL's first CteScan fetch is the run-ahead tracer's to judge where pg_demand never_run names its source (lowerv4 gate_stamp); guard: USES (read_on_demand and the CteScan shortcut only in subplan/mod.rs)",
    (G + "decompose/src/walk.rs", "Builder.ahead_licensed"): "the record of plan nodes granted an Ahead licence (gsets fold_ahead pushes it after licence())",
    (G + "exec/src/runner/keys.rs", "run_inits"): "runs a composed statement's InitPlan programs (keys.rs:9), and no lowering composes one: lower_composed makes no inits (lower/src/meeting.rs:24); guard: USES fails on an InitDesc literal anywhere",
    (G + "exec/src/runner/recheck.rs", "ahead_tagged"): "reads the run-ahead tag of a raised error (the licence's trace)",
    (G + "lower/src/rerun/words.rs", "Sequential.ahead"): "D2834's refusal flag: a reader took a deferred extern parameter ahead of its evaluation, and admission refuses",
    (G + "lowerv4/src/dict/strategies/code_cut.rs", "Pre.null_ahead"): "a NULL that sorts ahead of every value (an order, not a time)",
    (G + "lowerv4/src/effects/visible/group.rs", "folds_ahead"): "D1371: aggregates folded in PostgreSQL's transno order ahead of the group's finals (an order inside one node's work)",
    (G + "lowerv4/src/face/consts.rs", "RtConst::SkipAhead"): "a merge join's sorted side skipping to the other side's key (it reads fewer rows, never earlier)",
    (G + "lowerv4/src/face/fold.rs", "fold_ahead"): "D1371's fold emitter (folds_ahead's order)",
    (G + "lowerv4/src/face/source.rs", "DecodeOrder::Eager"): "decoding stored lanes at a window's open runs no user code and raises no data error",
    (G + "lowerv4/src/frame/instr.rs", "ahead"): "instrumentation: positions counted ahead of the read (EXPLAIN ANALYZE counters)",
    (G + "lowerv4/src/frame/instr.rs", "uncount_ahead"): "instrumentation: counts taken back at a write-back",
    (G + "lowerv4/src/frame/on_demand.rs", "eager_reader"): "picks the reader of an Init flagged INIT_EAGER (on_demand.rs:90-99), a flag set only with the Init's licence (lowerv4 ops/subplan/initplan/lower.rs:202-211, Init.ahead); guard: USES (INIT_EAGER only in on_demand.rs and initplan/lower.rs)",
    (G + "lowerv4/src/frame/prefetch.rs", "row_ahead"): "a CPU prefetch hint's row index",
    (G + "lowerv4/src/frame/prefetch_fp.rs", "ahead"): "a CPU prefetch hint",
    (G + "lowerv4/src/frame/px.rs", "Px.whole_read"): "the guard list: pipelines served to whole readers only (PortalKind::partial refuses the rest)",
    (G + "lowerv4/src/frame/px.rs", "ahead_of_rows"): "a fact run_ahead asks (the pipeline runs whole before any row can leave)",
    (G + "lowerv4/src/frame/px.rs", "ahead_tag"): "makes the run-ahead tag of a pipeline ahead_unchecked licensed",
    (G + "lowerv4/src/frame/px.rs", "kept_ahead"): "a fact ahead_of_rows asks",
    (G + "lowerv4/src/lifetime/material.rs", "refilled_ahead"): "a fact: D1107's eager refill; make_end holds its licence in Fill.ahead or refuses",
    (G + "lowerv4/src/memo.rs", "Memo.ahead_sets"): "the memo of ahead_of_rows's fact",
    (G + "lowerv4/src/ops/agg/numeric/class_num_varlena.rs", "fold_ahead"): "D1371's fold emitter for numeric varlena states (folds_ahead's order)",
    (G + "lowerv4/src/ops/agg/trans/class_stats.rs", "fold_ahead"): "D1371's fold emitter, handed to the class folding the call (class.num_varlena's hosted run; a register form keeps no run) (folds_ahead's order)",
    (G + "lowerv4/src/ops/agg/trans/plain_drain.rs", "Drain.ahead"): "D1371: the folds folds_ahead orders (an order inside one node's work)",
    (G + "lowerv4/src/ops/ctescan/lower.rs", "eager_resets"): "declares the eager CTE store's count and pull words as owner-run resets (lower.rs:563-576, Scope::OwnerRun: PostgreSQL's tuplestore_clear at ExecReScanCteScan) and runs no plan code; guard: private to its file, one call (lower.rs:550)",
    (G + "lowerv4/src/ops/material/lower.rs", "eager"): "a reader (lower.rs:158): a Reader with no child over a settled union's record, which the union's arms fill under rerun/union_arms.rs's own rows (admit, source); guard: USES (only union_arms.rs names material::lower::eager)",
    (G + "lowerv4/src/ops/material/lower.rs", "eager_fresh"): "eager's reader over a held union's record (lower.rs:165), emptying it at its open when no feed opened in the run, as ExecReScanMaterial clears its store; guard: USES (only union_arms.rs names material::lower::eager_fresh)",
    (G + "lowerv4/src/ops/srf/reopen_once.rs", "eager_build"): "the guard: a FROM store beside an eager outer-first build refuses",
    (G + "lowerv4/src/ops/subplan/site.rs", "Lend::Eager"): "evaluated at its step, which sits right before the SubPlan's first reader (decompose ops/shared/subplan.rs:61-63), so on the rows PostgreSQL's reader reaches; a conditional place or a VOLATILE one read twice (site.rs:178-179), or one after a raising or acting node (per_row.rs:510, 624), is Lend::Row; guard: USES (Lend::Eager only in site.rs and per_row.rs)",
    (G + "lowerv4/src/rerun/params_key.rs", "ahead"): "the guard (D2070): refuses a computed index key on a nest inner whose outer's source or ops before the nest act (OUTER_CALLS_AHEAD); it runs nothing itself",
    (G + "lowerv4/src/rerun/spool/form.rs", "eager_licensed"): "a fact: the Material fill's eager form; make_end holds its licence in Fill.ahead or refuses",
    (G + "lowerv4/src/rerun/spool/form.rs", "refill_ahead"): "a form guard of spool_fill: the refill it picks runs under the licence make_end holds in Fill.ahead",
    (G + "lowerv4/src/rerun/store_read.rs", "ahead_quiet"): "a fact: what PostgreSQL evaluates ahead of a SubPlan inside an expression raises nothing",
    (G + "lowerv4/src/rerun/union_arms.rs", "Ask::Ahead"): "a question (union_arms.rs `Ask`, private to the file): `admit` asks whether a union's feeds may fill it ahead of PostgreSQL's pulls, and the only answer, `answer`, replies with Px::ahead_unchecked's licence (frame/px.rs), held on each feed and its feeders and tagged (AheadTag); a feed it denies makes `admit` refuse by name (LOUD_UNION, HELD_UNION_AHEAD); it runs nothing itself; guard: USES (Ask::Ahead only in union_arms.rs)",
    (G + "lowerv4/src/step/expr_saop.rs", "attach"): "the claim (expr_saop.rs:260): it builds no set; a hashed constant's set built at bind holds its licence in the variant Arr::Set (expr_saop.rs:61; None: a user key's set, built at its first probe)",
    (G + "lowerv4/src/table/rec.rs", "GroupRec.ahead"): "D1371: the aggnos folds_ahead orders (an order inside one node's work)",
}

# The uses a NOT_AHEAD reason rests on, (file, name) -> [(pattern, the files where it may match)]: a reader that runs
# nothing because only these files reach it, a producer that only these files make. A match of the pattern in another file
# of RH's scope fails: the reason is read again before the new use lands (list the file, or hold a Licence).
_READER_OF_UNION = {G + "lowerv4/src/rerun/union_arms.rs"}
USES = {
    (G + "decompose/src/graph.rs", "eager_mask"): [(r"\beager_mask\b", {G + "decompose/src/graph.rs", G + "lower/src/inputs/key.rs"})],
    (G + "decompose/src/graph.rs", "eager_initplans"): [
        (r"\beager_initplans\b", {G + "decompose/src/graph.rs", G + "lower/src/inputs/key.rs"}),
        (r"\bcost_decide\b", {G + "lower/src/inputs/key.rs", G + "lowerv4/src/ops/scan/pgrc/lower.rs"})],
    (G + "decompose/src/ops/shared/breaker.rs", "spool_run"): [
        (r"\bspool_run\b", {G + "decompose/src/ops/shared/breaker.rs", G + "decompose/src/join.rs"})],
    (G + "decompose/src/ops/subplan/mod.rs", "read_on_demand"): [
        (r"\bread_on_demand\b", {G + "decompose/src/ops/subplan/mod.rs"}),
        (r"\bbreaker::spool\(\s*b\s*,\s*\w+\s*,\s*Kind::CteScan\b", {G + "decompose/src/ops/subplan/mod.rs"})],
    (G + "exec/src/runner/keys.rs", "run_inits"): [(r"(?<!struct )\bInitDesc\s*\{", set())],
    (G + "lowerv4/src/frame/on_demand.rs", "eager_reader"): [
        (r"\bINIT_EAGER\b", {G + "lowerv4/src/frame/on_demand.rs", G + "lowerv4/src/ops/subplan/initplan/lower.rs"})],
    (G + "lowerv4/src/ops/material/lower.rs", "eager"): [(r"\b(?:material|super)::lower::(?:\{[^}]*)?\beager\b", _READER_OF_UNION)],
    (G + "lowerv4/src/ops/material/lower.rs", "eager_fresh"): [
        (r"\b(?:material|super)::lower::(?:\{[^}]*)?\beager_fresh\b", _READER_OF_UNION)],
    (G + "lowerv4/src/ops/subplan/site.rs", "Lend::Eager"): [
        (r"\bLend::Eager\b", {G + "lowerv4/src/ops/subplan/site.rs", G + "lowerv4/src/ops/subplan/per_row.rs"})],
    (G + "lowerv4/src/rerun/union_arms.rs", "Ask::Ahead"): [(r"\bAsk::Ahead\b", {G + "lowerv4/src/rerun/union_arms.rs"})],
}
_USES = [(k, re.compile(rx), where) for k, uses in USES.items() for rx, where in uses]

# Construct C2: the licence carriers a strategy's licence rides on from decompose to the lowering, (file, Struct.field) ->
# (pattern, the files): RH counts the field licensed by its type, and checks it is not only recorded: the pattern, the
# lowering's hold of it for the run-ahead tracer (lowerv4 `Px::hold`, the program's `Pipeline::licence` stamp), must match
# in one of the files while the field exists. A carrier no lowering holds fails.
CARRIED = {
    (G + "decompose/src/ops/shared/subplan.rs", "SubPlanDesc.ahead"): (r"\bd\.ahead\b[^;]*\bpx\.hold\(", {G + "lowerv4/src/ops/subplan/per_row.rs"}),
}
_CARRIED = [(k, re.compile(rx), where) for k, (rx, where) in CARRIED.items()]

_LICENCE = re.compile(r"\bLicence\b")
_STAMP = re.compile(r"\bLicence\b|\bAheadTag\b")
_ENUM = re.compile(r"\benum\s+([A-Za-z_]\w*)[^{;]*\{")
_STRUCT = re.compile(r"\bstruct\s+([A-Za-z_]\w*)[^{;(]*\{")
_ATTR = re.compile(r"^(?:\s*#\[[^\]]*\])*\s*")
_VARIANT = re.compile(r"([A-Z]\w*)")
_FIELD = re.compile(r"(?:pub(?:\([^)]*\))?\s+)?([a-z_]\w*)\s*:(.*)", re.S)


def in_scope(p):
    return p.endswith(".rs") and (any(p.startswith(G + c + "/src/") for c in _CRATES) or any(p.startswith(x) for x in _EXEC))


def camel(n):
    return set(re.findall(r"[A-Z][a-z0-9]*", n))


def snake(n):
    return set(n.split("_"))


def _close(t, i):
    depth = 0
    while i < len(t):
        c = t[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return -1


def _items(body):
    """-> [(offset, text)]: an enum's or struct's body split at its top-level commas."""
    out, depth, start = [], 0, 0
    for i, c in enumerate(body):
        if c in "({[<":
            depth += 1
        elif c in ")}]>" and not (c == ">" and i and body[i - 1] == "-"):
            depth -= 1
        elif c == "," and depth == 0:
            out.append((start, body[start:i]))
            start = i + 1
    out.append((start, body[start:]))
    return out


def defs(src):
    """-> [(kind, name, line, licensed)]: every enum variant (kind variant, `Enum::Variant`) and struct field (kind
    flag, `Struct.field`) `src` defines, with whether it holds a licence (a variant's payload names `Licence`; a field's
    type names `Licence` or `AheadTag`)."""
    t, out = src.joined("code"), []
    for rx, kind in ((_ENUM, "variant"), (_STRUCT, "flag")):
        for m in rx.finditer(t):
            a = m.end() - 1
            b = _close(t, a)
            if b < 0:
                continue
            for off, item in _items(t[a + 1:b - 1]):
                body = item[_ATTR.match(item).end():]
                if not body.strip():
                    continue
                if kind == "variant":
                    v = _VARIANT.match(body)
                    if not v:
                        continue
                    name, licensed = f"{m.group(1)}::{v.group(1)}", bool(_LICENCE.search(body))
                else:
                    f = _FIELD.match(body)
                    if not f:
                        continue
                    name, licensed = f"{m.group(1)}.{f.group(1)}", bool(_STAMP.search(f.group(2)))
                line = t.count("\n", 0, a + 1 + off + (len(item) - len(item.lstrip()))) + 1
                out.append((kind, name, line, licensed))
    return out


def worded(kind, name):
    """Whether a definition's own name says it runs ahead (the words of the module doc)."""
    if kind == "variant":
        return bool(VARIANT_WORDS & camel(name.split("::")[-1]))
    if kind == "flag":
        f = name.split(".")[-1]
        return bool(FLAG_WORDS & snake(f)) or f in FLAG_NAMES
    return bool(FN_WORDS & snake(name))


def sites(src):
    """-> [(kind, name, line, licensed)]: the run-ahead strategies `src` defines (see the module doc)."""
    t, p = src.joined("code"), src.path
    listed = STRATEGIES.get(p, {})
    skip = {n for (f, n) in NOT_AHEAD if f == p}
    out = [d for d in defs(src) if (worded(d[0], d[1]) or d[1] in listed) and d[1] not in skip]
    for name, line, (a, b) in functions(src):
        if not (worded("fn", name) or name in listed) or name in skip:
            continue
        sig_start = t.rfind("fn", 0, a)
        out.append(("fn", name, line, bool(_LICENCE.search(t, sig_start, a))))
    return out


def parse_debt(text):
    """-> ({(path, name): (line, owner, kind)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 5 or not all(x.strip() for x in f[:5]):
            bad.append((n, "a row is path<TAB>name<TAB>owner lane<TAB>kind<TAB>what runs early"))
            continue
        kind = f[3].strip()
        if kind not in KINDS:
            bad.append((n, f"kind {kind!r} is not one of {', '.join(sorted(KINDS))}"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), kind)
    return rows, bad


def check_tree(ctx):
    out, hits, names = [], {}, {}
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    listed = {p for p, _ in rows}
    for p in walk_sources(ctx.root):
        if not in_scope(p) or in_any(p, _HOME):
            continue
        t = ctx.read(p)
        if t is None or generated(t):
            continue
        src = Src(p, t)
        for (fp, name), rx, where in _USES:
            if p not in where:
                out += [(p, n, f"a use NOT_AHEAD's reason for {fp} `{name}` does not cover (USES: only "
                                f"{', '.join(sorted(where)) or 'no file'}): read that reason again, then list this "
                                f"file in USES or hold a Licence") for n in src.hits(rx)]
        found = sites(src)
        if p in listed:
            names[p] = {n for _, n, _, _ in found}
        for kind, name, line, licensed in found:
            if not licensed:
                hits.setdefault((p, name), (line, kind))
    out += [(*at(n), note) for n, note in bad]
    for (fp, name), rx, where in _CARRIED:
        t = ctx.read(fp)
        if t is None or generated(t) or name not in {d[1] for d in defs(Src(fp, t))}:
            continue
        held = any(Src(w, tw).hits(rx) for w in sorted(where) for tw in [ctx.read(w)] if tw is not None)
        if not held:
            out.append((fp, 1, f"licence carrier `{name}` is recorded but no lowering holds it for the run-ahead tracer "
                               f"(CARRIED: Px::hold of it in {', '.join(sorted(where))}): hold it there, or the strategy "
                               f"it carries is an AHEAD-DEBT row again"))
    for (p, name), (n, kind) in sorted(hits.items()):
        if (p, name) not in rows:
            out.append((p, n, f"{kind} `{name}` runs work ahead of PostgreSQL's first demand and holds no Licence; "
                              f"instead: {INSTEAD}"))
    for (p, name), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
        if (p, name) not in hits:
            if name in names.get(p, set()):
                ctx.notes.append(f"RH: {ledger_loc(at, n)}: {p} `{name}` ({owner}) holds a Licence now: delete the row")
            else:
                ctx.notes.append(f"RH: {ledger_loc(at, n)}: {p} `{name}` ({owner}) is not in its file: delete the row "
                                 f"once it is gone (an entry queued ahead may add it)")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bn for bp, bn in base if (bp, bn) not in rows}
        for (p, name), (n, owner, _) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, name) not in base and name not in removed:
                out.append((*at(n), f"{p} `{name}` ({owner}) added: the list only shrinks (hold a Licence instead)"))
    return out


PLANTED_TOML = (G + "lowerv4/src/ops/sort/planted.rs",
                "pub enum BuildWhen { Lazy, Eager }\n"
                "pub struct Fill { pub at_open: bool }\n"
                "fn eager_build(k: usize) -> bool { k > 0 }\n")
_CARRIER = (G + "decompose/src/ops/shared/subplan.rs",
            "use crate::effects::Licence;\npub struct SubPlanDesc { pub plan_id: i32, pub ahead: Option<Licence> }\n")
MORE_PLANTED_TOML = (
    (G + "lowerv4/src/ops/sort/planted_use.rs", "fn eager_bit(arg: u64) -> bool { arg & m8::INIT_EAGER != 0 }\n"),
    (G + "decompose/src/ops/planted.rs", "pub struct Form { pub read_ahead: Option<u16> }\n"),
    (G + "exec/src/admit/planted.rs", "fn prefill(k: usize) -> usize { k }\n"),
    {"files": [_CARRIER, (G + "lowerv4/src/ops/subplan/per_row.rs", "fn make(px: &mut Px, d: &SubPlanDesc) { let _ = d.plan_id; }\n")],
     "want": "is recorded but no lowering holds it"},
    (G + "lowerv4/src/ops/subplan/planted_cte.rs", "fn store(b: &mut B, open: Open) { let _ = read_on_demand(b, 1); }\n"),
    (G + "lowerv4/src/rerun/planted_ask.rs", "fn fill(stops: Stops<'_>, feeds: &[usize]) -> bool { stops(Ask::Ahead(feeds)) }\n"),
)
CLEAN_TOML = (G + "lowerv4/src/ops/sort/clean.rs",
              "use gravity_decompose::effects::Licence;\n"
              "pub enum BuildWhen { Lazy, Eager(Licence), Ahead { licence: Licence, node: u32 } }\n"
              "pub struct Fill { pub at_open: Option<Licence>, pub ahead: Option<gravity_program::AheadTag> }\n"
              "fn eager_build(k: usize) -> Option<Licence> { None }\n"
              "fn run_eager(l: &Licence, k: usize) -> usize { k }\n")
MORE_CLEAN_TOML = (
    (G + "lowerv4/src/frame/on_demand.rs", "fn read_bit(arg: u64) -> bool { arg & m8::INIT_EAGER != 0 }\n"),
    (G + "hosts/src/ops/scan/clean.rs", "pub enum ReadWhen { Eager }\npub struct W { pub ahead: bool }\n"),
    {"files": [_CARRIER, (G + "lowerv4/src/ops/subplan/per_row.rs", "fn make(px: &mut Px, k: usize, d: &SubPlanDesc) {\n    if let Some(l) = d.ahead {\n        px.hold(k, &l);\n    }\n}\n")]},
    (G + "lowerv4/src/rerun/union_arms.rs", "enum Ask<'a> { Quiet(&'a [usize]), Ahead(&'a [usize]) }\n"
                                            "fn answer(px: &mut Px, a: Ask<'_>) -> bool { matches!(a, Ask::Ahead(_)) }\n"),
)
