"""The rules' one exempt table: a hit that is not what its rule means, named by rule, file and symbol, with its reason
and its test. No wildcard, no directory, file or module row: a row's symbol is the innermost fn, struct, enum, union,
trait, const, static or type item that declares or holds the hit line (an impl or a mod never names one, so a hit
outside any such item cannot be exempted).

Each row's test is a source text that hits the rule inside the row's symbol. The selftest checks that it passes at the
row's file, and that it hits under another symbol name and in another file. A row that excuses no hit on the tree
fails the full check (delete the row), as a LEGACY line whose directory passes every rule does."""
import re
from common import G

# (rule, file, symbol, reason, test)
ROWS = [
    ("R3", G + "backend/tpde/src/frontend.rs", "FIXED_ASSIGNMENTS",
     "the register allocator's fixed assignment counts per class (fourteen general, six vector), not OIDs",
     "impl Alloc {\n    const FIXED_ASSIGNMENTS: [u32; 2] = [14, 6];\n}\n"),
    ("R11", G + "backend/src/cache.rs", "TARGET",
     "the target ISA's tag in the code cache's content key (0: neither aarch64 nor x86_64), not a size or threshold",
     "const TARGET: u64 = 0;\n"),
    ("R11", G + "ir/src/text.rs", "MAX_SEXP_DEPTH",
     "the IR text grammar's own nesting bound (six lists; the reader's cap against a recursion off the stack), not a "
     "tuning threshold; gravity_ir has no dependencies, so contract::layout (gravity_abi) is out of its reach",
     "pub const MAX_SEXP_DEPTH: u32 = 64;\n"),
    ("C29", G + "exec/src/statement.rs", "Facts",
     "the unit Protocol marker for ProtoKind::Facts (an execution's facts words), not a plan or catalog facts type",
     "struct Facts;\n"),
    ("C29", G + "lowerv4/src/props/props.rs", "Props",
     "admission's per-graph set of needed kinds and property tokens, computed once per statement, not a plan or "
     "catalog facts type",
     "pub struct Props {\n    pub max_rows: u64,\n}\n"),
    ("C29", G + "decompose/src/corpus.rs", "FixtureFacts",
     "test double for corpus fixtures",
     "pub struct FixtureFacts { pub rels: Vec<u32> }\n"),
    ("C29", G + "hosts/src/ops/scan/pgrc/granule_source.rs", "ZoneFacts",
     "a pgrc Stats record's wire fields read at run time, not plan or catalog facts",
     "struct ZoneFacts {\n    lo: i64,\n}\n"),
    ("C4", G + "prewarm/src/lib.rs", "warm_relation",
     "a syscache warm before the first statement: the result is dropped, no type property is derived",
     "fn warm_relation(relid: Oid) -> PgResult<u64> {\n    lsyscache::typ::get_typlenbyval(a.atttypid)?;\n    Ok(1)\n}\n"),
    ("C30", G + "lower/src/inputs/key.rs", "pipeline_body",
     "the content key hashes the Limit op as it hashes every op (the plan's identity); no bound read steers a plan",
     "fn pipeline_body(h: &mut Fnv128, op: &Op<'_>) {\n    match op {\n        Op::Limit { count, offset, ties } => h.limit(count, offset, ties),\n"
     "        _ => {}\n    }\n}\n"),
    ("C30", G + "lowerv4/src/graph.rs", "step_kind",
     "the op-to-step-kind map names the Limit op's kind; no bound is read",
     "pub fn step_kind(op: &Op<'_>) -> KindId {\n    match op {\n        Op::Limit { .. } => KindId::StepLimit,\n        _ => KindId::Step,\n    }\n}\n"),
    ("C30", G + "lowerv4/src/effects.rs", "node_ok",
     "the effects walk over plan nodes asks whether a WITH TIES Limit's tie operators call user code; no bound is read",
     "fn node_ok(n: Node<'_>) -> bool {\n    n.as_limit().is_some_and(|l| l.limitOption != LimitOption::LIMIT_OPTION_WITH_TIES)\n}\n"),
    # RL2 (C6 (e)): a limit-named panic no input reaches, or that only the lowerer reaches (lowerv4's catch makes a
    # lowering panic a refusal: PostgreSQL runs the statement, the server lives)
    ("RL2", G + "abi/src/breaker.rs", "build",
     "the narrow record tier is chosen by fits_inline_at, which counts the appended count sentinel (nslots + 1 <= "
     "MAX_RECORD_SLOTS); the run-time rebuild (kernels ops/sort/topn/t3.rs) re-lays a shape the "
     "lowerer fitted, and the fixed shapes are constants",
     "const fn build(keys: &[KeyKind], slots: &[SlotKind]) -> RecordLayout {\n    assert!(slots.len() < MAX_RECORD_SLOTS, "
     "\"no room for the count sentinel\");\n}\n"),
    ("RL2", G + "abi/src/breaker.rs", "compute_at",
     "lowering only: the wide tier is laid by program record_layout_at (owner.rs), which the lowerer calls under "
     "lowerv4's catch, so a shape past the u16 ids is a refusal, never an abort",
     "fn compute_at(keys: &[KeyKind]) -> WideLayout {\n    assert!(keys.len() <= u16::MAX as usize, \"the u16 ids are the "
     "sanity limit\");\n}\n"),
    ("RL2", G + "abi/src/host/wasm32.rs", "call",
     "the argument count and narrow mask come from the host table's declaration (at most MAX_ARGS), which the code "
     "generator emits; no statement chooses them",
     "unsafe fn call(n: usize, m: u32) -> u64 {\n    panic!(\"{n} arguments: past MAX_ARGS\")\n}\n"),
    ("RL2", G + "exec/src/ops/subplan/keyset.rs", "put",
     "insert grows the table first, so the set is at most half full and the bounded walk always meets an empty tag",
     "fn put(keys: &mut [u64]) -> bool {\n    unreachable!(\"the key set is at most half full\")\n}\n"),
    ("RL2", G + "ir/src/promote.rs", "promote",
     "lowering only, and its one caller (lowerv4 frame/instr.rs) carries at most 64 offsets (`offs.len() < 64`)",
     "pub fn promote(cells: &[Cell]) {\n    assert!(cells.len() <= 64, \"promote: at most 64 cells\");\n}\n"),
    ("RL2", G + "program/src/program/owner.rs", "arg_fold",
     "lowering only, and its one caller (lowerv4 ops/group/pairs.rs) folds a Pair's `folds: [Option<SlotKind>; 3]`, so "
     "at most three",
     "pub const fn arg_fold(mut self, slot: u8) -> OwnerDesc {\n    assert!(self.n < 3, \"at most three argument "
     "folds\");\n    self\n}\n"),
    ("RL2", G + "program/src/program/owner.rs", "codes",
     "a descriptor builder only the lowerer would call (no caller today): under lowerv4's catch a run past eight code "
     "columns is a refusal, never an abort",
     "pub const fn codes(mut self, codes: u32) -> TopKDesc {\n    assert!(codes.count_ones() <= 8, \"at most eight code "
     "columns\");\n    self\n}\n"),
    ("RL2", G + "program/src/program/site.rs", "new",
     "an output column's name is PostgreSQL's (a target entry's resname), which the parser truncates to NAMEDATALEN - 1 "
     "(63) bytes, below NAME_LEN (64); its callers are the lowerer's and the hand-built programs'",
     "pub fn new(name: &str) -> OutCol {\n    assert!(name.len() < NAME_LEN, \"column name too long\");\n}\n"),
]

BY_FILE = {}
for _row in ROWS:
    BY_FILE.setdefault((_row[0], _row[1]), {})[_row[2]] = _row

_VIS = r"^\s*(?:pub(?:\([^)]*\))?\s+)?"
_DECL = [re.compile(_VIS + r"(?:(?:const|async|unsafe|default|extern(?:\s*\"\")?)\s+)*fn\s+([A-Za-z_]\w*)"),
         re.compile(_VIS + r"(?:unsafe\s+)?(?:struct|enum|union|trait|type)\s+([A-Za-z_]\w*)"),
         re.compile(_VIS + r"(?:const|static)\s+(?:mut\s+)?(?!fn\b)([A-Za-z_]\w*)\s*:")]
_SCOPE = re.compile(_VIS + r"(?:unsafe\s+)?(?:impl\b|mod\s+\w+\s*\{)")
_BRACE = re.compile(r"[{};()\[\]]")


def _decl(line):
    """-> the item name a code line declares, None for an impl / mod line, False for neither."""
    for rx in _DECL:
        m = rx.match(line)
        if m:
            return m.group(1)
    return None if _SCOPE.match(line) else False


def symbols(src, lines):
    """-> {line: the innermost named item that declares or holds it, or None} for the 1-based `lines` of `src`."""
    want, out = set(lines), {}
    stack, pending, depth, paren, at = [], False, 0, 0, (0, 0)   # stack: (name or None, depth inside its block)
    for n, line in enumerate(src.code, 1):
        d = _decl(line)
        if n in want:
            out[n] = d if d else (stack[-1][0] if stack else None)
            if len(out) == len(want):
                break
        if d is not False:
            pending, at = d, (depth, paren)
        for b in _BRACE.finditer(line):
            c = b.group()
            if c == "{":
                depth += 1
                if pending is not False:
                    stack.append((pending, depth))
                    pending = False
            elif c == "}":
                while stack and stack[-1][1] >= depth:
                    stack.pop()
                depth -= 1
            elif c in "([":
                paren += 1
            elif c in ")]":
                paren -= 1
            elif pending is not False and (depth, paren) == at:   # a body-less item: `struct Facts;`, `fn f();`
                pending = False
    return out


def excused(rule_id, src, hits, used=None):
    """-> the hits no row excuses; each row that excuses one is added to `used`."""
    rows = BY_FILE.get((rule_id, src.path))
    if not rows or not hits:
        return hits
    syms = symbols(src, hits)
    keep = []
    for n in hits:
        row = rows.get(syms.get(n))
        if row is None:
            keep.append(n)
        elif used is not None:
            used.add(row[:3])
    return keep


_IDENT = re.compile(r"^[A-Za-z_]\w*$")


def row_problems(rule_ids):
    """-> [problem] with the table's own form: a known rule, a .rs file under crates/, an identifier symbol, a reason,
    a test that names its symbol, no duplicate."""
    bad, seen = [], set()
    for row in ROWS:
        rid, path, sym, reason, test = row
        key = f"{rid} {path}:{sym}"
        if rid not in rule_ids:
            bad.append(f"{key}: no rule {rid}")
        if not (path.startswith("crates/") and path.endswith(".rs")) or "*" in path:
            bad.append(f"{key}: a row names one .rs file")
        if not _IDENT.match(sym):
            bad.append(f"{key}: a row names one symbol (an identifier)")
        if not reason.strip():
            bad.append(f"{key}: no reason")
        if not re.search(r"\b" + re.escape(sym) + r"\b", test):
            bad.append(f"{key}: the test does not name its symbol")
        if row[:3] in seen:
            bad.append(f"{key}: duplicate row")
        seen.add(row[:3])
    return bad


def renamed(row):
    """The row's test with its symbol renamed (the hit is then outside the row)."""
    return re.sub(r"\b" + re.escape(row[2]) + r"\b", "X_" + row[2], row[4])


def elsewhere(row):
    """Another file of the row's directory."""
    return row[1].rsplit("/", 1)[0] + "/exempt_elsewhere.rs"
