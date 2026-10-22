"""RQ: a host step's work not polled through cancelpoll's Step (C9, reports/STEP2-CONSTRUCTS-C8-C9-C10-20261012.md C9).
A host step that runs unbounded work (rows, bytes, parts, partitions) without polling its statement's abort byte holds a
canceled statement's thread for seconds; each fix used to add a local helper with a stride of its own (pread_pieces,
zero_polled, push_polled, slot_poll, sort_polled, row_poll, POLL_ENTRIES, POLL_ROWS, pin_part_polled, merge's
`cancelled`). The home is `cancelpoll::Step` (crates/_support/cancelpoll/src/step.rs): `step(label)` / `step_on(byte,
label)`, then `tick()` per unit, `each(iter)`, `pieces(len, PIECE, f)`, `batch(i)` (units of ~100 ns or less) and
`cancelpoll::sort_by`. RQ finds, charged to the innermost function:
- byte: a poll of the abort byte outside the home: `cancelpoll::canceled()` / `polled()`, gravity_abi `abort_set` /
  `abort_byte_set` (every production Gravity source and pgrc_read), except the control-plane stops listed in
  lint/POLL-DEBT/README (CONTROL below): a statement's stop between phases, workers or claims, and the kernels' poll;
- phase: a `cancelpoll::phase(..)` label (a Step labels and polls);
- loop: in host-step code (HOST), a `for` / `while` / `loop` or a whole-slice `.sort*(..)` that neither ticks a Step
  (its own text holds a tick, or it calls a function of its crate that RQ found ticking, by its kind: a method that
  takes `self` as `.name(`, a free or associated function as `name(` / `path::name(`) nor carries a marker
  `// bounded("<category>: <why>")` on its line or the line above; a loop over a fixed domain (a range to a literal or
  an ALL-CAPS constant, an array literal, `.take(CONST)`) needs neither. Categories: const-domain, catalog-sized,
  key-columns, small-fixed, polled-callee:<fn> (a function RQ sees ticking), other (a sentence a reviewer can check);
- held: a cancelpoll `Step` in a struct, a static or a return type (a Step is a stack guard of one kernel call): fails.
On the statement's leader thread PostgreSQL's CHECK_FOR_INTERRUPTS (`cancelpoll::interrupts::check()`) polls too: in a
host of LEADER (leader-only, each with its proof, listed in lint/POLL-DEBT/README) a loop is polled when every pass calls
`check()?` (or `if let Err(..) = check()`), or a function of the same LEADER host that calls it on every path (`name(..)?`,
all by name and kind as for a tick). Every pass: in a `while` header, or in the loop's body outside any nested block, with
no `continue` before it and no `&&`, `|` or `else` in its statement; on every path: in the function's body outside any
nested block, with no `return` before it. Off the leader check() reads the pool thread's own interrupt state, which no
cancel sets (a no-op): a loop outside LEADER that would rest on it fails.
A site not moved yet needs a row in lint/POLL-DEBT (a directory, one row per file): path, function, kind, count, owner
lane, why it stays; an unlisted one or a count above its row FAILS. On a landing (--base) a row the parent lacks, or a
count above the parent's, fails (the list only shrinks), unless it replaces a row of the same function and kind the
landing removes (a move). A row above its tree count is a note (lower it); a row whose sites are gone, a note (delete it).
`rq_polls.py --list-bounded [--root DIR]` prints every marker and the counts per category.
The kernels crate (ABI only, no cancelpoll) polls only through its one face, kernels ops/sort/polled.rs (D4140); RQ does
not scan it yet."""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import G, ledger_loc, Src, generated, walk_sources  # noqa: E402
from rl_lifetime import _body_brace, _close, _FN, functions  # noqa: E402

ID = "RQ"
TITLE = "a host step's work not polled through cancelpoll's Step"
INSTEAD = "cancelpoll::step(label) + tick() / each() / pieces() / batch() / cancelpoll::sort_by " \
          "(crates/_support/cancelpoll/src/step.rs), or // bounded(\"<category>: <why>\") on a bounded loop; " \
          "not yet moved: a lint/POLL-DEBT row"
GLOBAL = True
DEBT = G + "lint/POLL-DEBT"
PGRC_READ = "crates/backend/access/pgrcolumnar/pgrc_read/src/"
PGRC_TEST = (PGRC_READ + "tests/", PGRC_READ + "testpart.rs")

# host-step code: where every loop is judged
HOST = (G + "driver/src/hostimpl/", G + "driver/src/mem/spill.rs", G + "driver/src/mem/spill_host.rs",
        G + "driver/src/mem/batch.rs",
        G + "driver/src/mem/runcodec.rs", G + "driver/src/section.rs", G + "driver/src/section/", G + "hosts/src/ops/",
        G + "hosts/src/dict/text_hosts.rs", G + "parallel/src/merge.rs", G + "pool/src/", G + "exec/src/legs.rs",
        PGRC_READ)

# the control-plane stops (path, function): the statement's own stop between pipelines, claims, sections and workers,
# and the kernels' poll; none is a host step's poll. Listed in lint/POLL-DEBT/README.
CONTROL = {
    (G + "driver/src/entry.rs", "run_pipeline_at"), (G + "driver/src/entry.rs", "claim_at"),
    (G + "driver/src/entry.rs", "out_of_order"), (G + "driver/src/entry.rs", "run_ahead"),
    (G + "driver/src/entry.rs", "pull_nested"), (G + "exec/src/glue.rs", "service_interrupts"),
    (G + "exec/src/glue/raw.rs", "poll_kernel"), (G + "parallel/src/lib.rs", "run_section"),
    (G + "parallel/src/lib.rs", "own_run_end"), (G + "parallel/src/worker.rs", "run_claim"),
}
CONTROL_HOME = (G + "abi/src/",)          # the abort byte's own reader (gravity_abi ctx: abort_set, poll)

# the leader-only hosts (path prefix, function or None, the host's own spelling of the check or None, why it is
# leader-only), where cancelpoll::interrupts::check() polls; listed in lint/POLL-DEBT/README
LEADER = (
    (G + "hosts/src/ops/window/", None, None,
     "window pipelines are leader-only: decompose leaves the fill Ineligible{Sink} and the reader site-less, and "
     "frame.rs keys_match debug-asserts no pool thread runs it (D1368)"),
    (G + "hosts/src/ops/srf/store.rs", None, None,
     "ScanStore::check, at every batch, refuses a read off the thread that built the store, the leader"),
    (G + "hosts/src/ops/modifytable/", None, None,
     "a write's pipeline is serial (decompose serial_after(.., Reason::Write)) and a row op runs leader-only with no "
     "pool wake (lowerv4 modifytable/rowop.rs)"),
    (G + "hosts/src/ops/agg/hosts.rs", "admit_set", None,
     "exec's after_pipeline hooks call it (keyed.twopass, join.domain): the leader's serial lay-out of pass 1's keys"),
    (G + "pool/src/lib.rs", "run_led", r"\(\s*self\.ambient\.interrupts\s*\)\s*\(\s*\)",
     "the section's leader parks in it (parallel run_section, exec leaf_fan_out, pool end_fan_out); "
     "Ambient::SERVER.interrupts is cancelpoll::interrupts::check"),
)

CATEGORIES = ("const-domain", "catalog-sized", "key-columns", "small-fixed", "polled-callee", "other")

_BYTE = re.compile(r"\bcancelpoll::(?:canceled|polled)\s*\(|(?<![\w.])(?:\w+::)*abort_(?:byte_)?set\s*\(")
_PHASE = re.compile(r"\bcancelpoll::phase\s*\(")
_NEEDS = re.compile(r"cancelpoll::|abort_set|abort_byte_set")
_LOOP = re.compile(r"\b(?:for\b(?=[^{;]*\bin\b)|while\b|loop\s*\{)|\.sort(?:_unstable)?(?:_by(?:_key|_cached_key)?)?\s*\(")
_TICK = re.compile(r"\.(?:tick|batch|pieces|each)\s*\(|(?<![\w.])(?:cancelpoll::)?sort_by\s*\(\s*&mut\b")
_FIXED = re.compile(r"^for\b.*?\bin\s+(?:\(?\s*(?:\d+|[A-Z][A-Z0-9_]*)(?:u\d+|usize)?\s*\.\.=?\s*(?:\d+(?:u\d+|usize)?"
                    r"|[A-Z][A-Z0-9_]*(?:\s*as\s+\w+)?|\d+(?:u\d+|usize)?\s*<<\s*[A-Z][A-Z0-9_]*)\s*\)?"
                    r"|&?\[[^\]]*\](?:\s*\.\s*(?:into_iter|iter)\(\))?(?:\.\w+\(\))*"
                    r"|.*\.take\(\s*(?:\d+|[A-Z][A-Z0-9_]*)\s*\))\s*$")
_MARK = re.compile(r"\bbounded\(\s*\"([^\"]*)\"\s*\)")
_STEP_USE = re.compile(r"\buse\s+cancelpoll::(?:\{[^}]*\bStep\b[^}]*\}|Step\b)")
_HELD_TYPE = re.compile(r"\bcancelpoll::Step\b")
_STRUCT = re.compile(r"\bstruct\s+\w+[^{;]*\{")
_STATIC = re.compile(r"\bstatic\s+(?:mut\s+)?\w+\s*:[^=;]*\bStep\b|thread_local!\s*\{[^}]*\bStep\b", re.S)
_RET = re.compile(r"->\s*(?:cancelpoll::)?Step\b")
_CHECK = re.compile(r"(?<![\w.])(?:cancelpoll::)?interrupts::check\s*\(\s*\)")
_ERR_LET = re.compile(r"\bif\s+let\s+Err\s*\([^()]*\)\s*=\s*$")
_SELF = re.compile(r"\(\s*(?:&\s*(?:'\w+\s+)?)?(?:mut\s+)?self\b")
_TURBO = r"\s*(?:::\s*<[^;{}()]*>\s*)?\("
# a POLL-DEBT row the landing that made the call match precise adds (the loops the name-only match credited with a call
# they don't make) says so in its why; only that landing, whose parent's RQ still matched by name, admits it
PRECISE_ROW = "RQ precise match"


def host(path):
    return path.startswith(HOST)


def crate_of(path):
    if path.startswith(PGRC_READ):
        return "pgrc_read"
    return path[len(G):].split("/", 1)[0] if path.startswith(G) else None


def sources(ctx):
    """-> the production sources RQ reads: Gravity's, and pgrc_read's (its test modules out)."""
    out = list(walk_sources(ctx.root))
    base = os.path.join(ctx.root, PGRC_READ)
    if os.path.isdir(base):
        for dp, dn, fn in os.walk(base):
            dn[:] = sorted(d for d in dn if d != "target")
            for f in sorted(fn):
                if f.endswith(".rs"):
                    p = os.path.relpath(os.path.join(dp, f), ctx.root).replace(os.sep, "/")
                    if not p.startswith(PGRC_TEST):
                        out.append(p)
    return out


def methods(code):
    """-> {body start: whether the function takes `self`} for every function `code` defines with a body (as
    rl_lifetime.functions finds them)."""
    out = {}
    for m in _FN.finditer(code):
        i = m.end()
        while i < len(code) and code[i].isspace():
            i += 1
        if i < len(code) and code[i] == "<":
            i = _close(code, i, "<", ">")
            while 0 <= i < len(code) and code[i].isspace():
                i += 1
        if i < 0 or i >= len(code) or code[i] != "(":
            continue
        j = _close(code, i, "(", ")")
        brace = _body_brace(code, j) if j >= 0 else -1
        if brace >= 0:
            out[j + brace] = bool(_SELF.match(code, i))
    return out


def calls(tick):
    """-> a regex of a call of a ticking function {(name, method)} by its kind, or None: a method `.name(`, a free or
    associated function `name(` / `path::name(` (a turbofish allowed)."""
    alts = []
    for meth, pre in ((True, r"\.\s*"), (False, r"(?<![\w.])")):
        names = sorted(re.escape(n) for n, m in tick if m == meth)
        if names:
            alts.append(pre + "(?:" + "|".join(names) + ")" + _TURBO)
    return re.compile("|".join(alts)) if alts else None


def leader(path, fn):
    """-> the index of the LEADER host `fn` of `path` is in, or None."""
    for i, (pre, f, _, _) in enumerate(LEADER):
        if path.startswith(pre) and (f is None or f == fn):
            return i
    return None


def _every(code, lo, at, guard):
    """A call at `at`, below the block that opens at `lo`, runs whenever the block's text before it runs: outside any
    nested block, no `guard` (continue, return) before it, and no `&&`, `|` or `else` in its statement."""
    pre = code[lo + 1:at]
    if pre.count("{") != pre.count("}") or re.search(r"\b" + guard + r"\b", pre):
        return False
    stmt = pre[max(pre.rfind(";"), pre.rfind("{"), pre.rfind("}")) + 1:]
    return not ("&&" in stmt or "|" in stmt or re.match(r"\s*else\b", stmt))


def _raised(code, m, check):
    """The call `m` hands its error on: `..)?`, or (a check) `if let Err(..) = check()`."""
    k = m.end() if check else _close(code, m.end() - 1, "(", ")")
    if k < 0:
        return False
    rest = code[k:k + 40].lstrip()
    if rest.startswith("?"):
        return True
    return check and bool(_ERR_LET.search(code[max(0, m.start() - 80):m.start()]))


def polls_every(code, lo, hi, rxs, guard, header=None):
    """Some check of `rxs` [(regex, is the check itself)] in the block code[lo:hi] (lo its brace) runs on every pass or
    path and hands its error on; `header` (a `while` header's start) counts too."""
    for rx, check in rxs:
        for m in rx.finditer(code, header if header is not None else lo, hi):
            if m.start() < lo:
                stmt = code[header:m.start()]
                if not ("&&" in stmt or "|" in stmt) and _raised(code, m, check):
                    return True
            elif _every(code, lo, m.start(), guard) and _raised(code, m, check):
                return True
    return False


def _owner(fns, i):
    inner = [(n, b - a) for n, ln, (a, b) in fns if a <= i < b]
    return min(inner, key=lambda x: x[1])[0] if inner else "<item>"


def _line(code, i):
    return code.count("\n", 0, i) + 1


def marker(src, ln):
    """-> (line, text) of the bounded marker on line `ln` or the line above (a comment-only line), else None."""
    for k in (ln, ln - 1):
        if 1 <= k <= len(src.comments):
            m = _MARK.search(src.comments[k - 1])
            if m and (k == ln or not src.code[k - 1].strip()):
                return k, m.group(1)
    return None


def parse_marker(text):
    """-> (category, callee, why) or (None, None, problem)."""
    head, sep, why = text.partition(": ")
    cat, _, callee = head.partition(":")
    if cat not in CATEGORIES:
        return None, None, f"category `{head}` is not one of {', '.join(CATEGORIES)} (bounded(\"<category>: <why>\"))"
    if (cat == "polled-callee") != bool(callee):
        return None, None, "polled-callee names its function (polled-callee:<fn>), no other category does"
    if not sep or not why.strip():
        return None, None, "a marker says why after its category (bounded(\"<category>: <why>\"))"
    if cat == "other" and len(why.split()) < 6:
        return None, None, "an `other` marker says why in a sentence a reviewer can check"
    return cat, callee or None, why.strip()


class Scan:
    """One pass over the sources: per-site hits, markers, the ticking functions per crate."""

    def __init__(self, ctx):
        self.hits = {}              # (path, fn, kind) -> [lines]
        self.fails = []             # (path, line, note)
        self.markers = []           # (path, line, category, callee, why)
        self.control_seen = set()
        files = []
        for p in sources(ctx):
            t = ctx.read(p)
            if t is None or generated(t):
                continue
            if not (_NEEDS.search(t) or host(p) or "Step" in t):
                continue
            files.append(Src(p, t))
        bodies = {}                 # crate -> {fn: [(body text, takes self)]}
        spans = {}
        units = {}                  # LEADER index -> {fn: [(code, body start, body end, takes self)]}
        for s in files:
            code = s.joined("code")
            fns = functions(s)
            spans[s.path] = fns
            if host(s.path) or _TICK.search(code):
                meth = methods(code)
                for n, ln, (a, b) in fns:
                    bodies.setdefault(crate_of(s.path), {}).setdefault(n, []).append((code[a:b], meth.get(a, False)))
                    u = leader(s.path, n)
                    if u is not None:
                        units.setdefault(u, {}).setdefault(n, []).append((code, a, b, meth.get(a, False)))
        # a name ticks when every function of its crate by that name ticks; a call of it matches the kinds its functions
        # have (a call names its kind, not its type): {(name, takes self)}
        self.ticking = {}
        for c, fb in bodies.items():
            kinds = {n: {m for _, m in bs} for n, bs in fb.items()}
            tick = {n for n, bs in fb.items() if all(_TICK.search(b) for b, _ in bs)}
            while True:
                rx = calls({(n, m) for n in tick for m in kinds[n]})
                more = {n for n, bs in fb.items() if n not in tick and rx and all(rx.search(b) for b, _ in bs)}
                if not more:
                    break
                tick |= more
            self.ticking[c] = {(n, m) for n in tick for m in kinds[n]}
        self.any_ticking = {n for t in self.ticking.values() for n, _ in t}
        # a LEADER host's checking functions: every one by that name in the host calls check() on every path (a fixpoint
        # over the calls of the ones found); per host, the checks a loop there may rest on [(regex, is the check)]
        self.checks = {}
        for u, (_, _, own, _) in enumerate(LEADER):
            base = [(_CHECK, True)] + ([(re.compile(own), True)] if own else [])
            fb = units.get(u, {})
            kinds = {n: {m for *_, m in bs} for n, bs in fb.items()}
            found = set()
            while True:
                rx = calls({(n, m) for n in found for m in kinds[n]})
                rxs = base + ([(rx, False)] if rx else [])
                more = {n for n, bs in fb.items() if n not in found
                        and all(polls_every(c, a, b, rxs, "return") for c, a, b, _ in bs)}
                if not more:
                    break
                found |= more
            rx = calls({(n, m) for n in found for m in kinds[n]})
            self.checks[u] = base + ([(rx, False)] if rx else [])
        for s in files:
            self._file(s, spans[s.path])

    def _add(self, p, f, kind, ln):
        self.hits.setdefault((p, f, kind), []).append(ln)

    def _file(self, s, fns):
        p, code = s.path, s.joined("code")
        for m in _BYTE.finditer(code):
            f = _owner(fns, m.start())
            if p.startswith(CONTROL_HOME) or (p, f) in CONTROL:
                self.control_seen.add((p, f))
                continue
            self._add(p, f, "byte", _line(code, m.start()))
        for m in _PHASE.finditer(code):
            self._add(p, _owner(fns, m.start()), "phase", _line(code, m.start()))
        self._held(s, code)
        marks = {}
        for k, c in enumerate(s.comments, 1):
            mm = _MARK.search(c)
            if mm:
                marks[k] = mm.group(1)
        used = set()
        if host(p):
            rx = calls(self.ticking.get(crate_of(p), set()))
            for m in _LOOP.finditer(code):
                ln = _line(code, m.start())
                j = k = -1
                if m.group(0).startswith("."):
                    span = code[m.start():m.end()]
                else:
                    # `loop {` matched its own brace; a `for` / `while` header runs up to its body's
                    j = m.end() - 1 if m.group(0).startswith("loop") else code.find("{", m.end())
                    k = _close(code, j, "{", "}") if j >= 0 else -1
                    if k < 0:
                        continue
                    hdr = " ".join(code[m.start():j].split())
                    span = code[m.start():k]
                    if _FIXED.search(hdr):
                        mk = marker(s, ln)
                        if mk:
                            used.add(mk[0])
                        continue
                mk = marker(s, ln)
                if mk:
                    used.add(mk[0])
                if _TICK.search(span) or (rx and rx.search(span)):
                    if mk:
                        self.fails.append((p, mk[0], "a bounded marker on a loop that ticks a Step: delete the marker"))
                    continue
                if j >= 0:
                    u = leader(p, _owner(fns, m.start()))
                    if u is not None and polls_every(code, j, k, self.checks[u], "continue",
                                                     m.start() if m.group(0) == "while" else None):
                        if mk:
                            self.fails.append((p, mk[0], "a bounded marker on a loop that runs "
                                                         "cancelpoll::interrupts::check() every pass: delete the marker"))
                        continue
                    if u is None and not mk and _CHECK.search(span):
                        self.fails.append((p, ln, "a loop that rests on cancelpoll::interrupts::check() outside RQ's "
                                                  "leader-only hosts (LEADER): off the statement's leader it reads the "
                                                  "pool thread's own interrupt state, which no cancel sets; tick a Step "
                                                  "(or add the host to LEADER with its proof, in the README too)"))
                        continue
                if mk:
                    cat, callee, why = parse_marker(mk[1])
                    if cat is None:
                        self.fails.append((p, mk[0], why))
                    elif callee and callee not in self.any_ticking:
                        self.fails.append((p, mk[0], f"polled-callee:{callee}: RQ sees no function `{callee}` ticking a Step"))
                    else:
                        self.markers.append((p, mk[0], cat, callee, why))
                    continue
                self._add(p, _owner(fns, m.start()), "loop", ln)
        for k in sorted(set(marks) - used):
            self.fails.append((p, k, "a bounded marker on no loop (it goes on the loop's line or the line above)"))

    def _held(self, s, code):
        if s.path.startswith("crates/_support/cancelpoll/"):
            return
        bare = bool(_STEP_USE.search(code))
        ty = re.compile(r"\b(?:cancelpoll::)?Step\b") if bare else _HELD_TYPE
        for m in _STRUCT.finditer(code):
            k = _close(code, m.end() - 1, "{", "}")
            if k > 0 and ty.search(code[m.end():k]):
                self.fails.append((s.path, _line(code, m.start()), "a struct holds a cancelpoll Step: a Step is a stack guard"))
        for m in _STATIC.finditer(code):
            if ty.search(m.group(0)):
                self.fails.append((s.path, _line(code, m.start()), "a static holds a cancelpoll Step: a Step is a stack guard"))
        for m in _RET.finditer(code):
            if bare or m.group(0).find("cancelpoll::") >= 0:
                self.fails.append((s.path, _line(code, m.start()), "a function returns a cancelpoll Step: open it where it ticks"))


def debt_file(path, name, kind):
    """A row's file name: <path below crates, '/' as '.'>@<function>@<kind>."""
    return path[len("crates/"):].replace("/", ".") + "@" + name + "@" + kind


def parse_debt(text):
    """-> ({(path, function, kind): (line, count, owner)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 6 or not all(x.strip() for x in f[:6]) or f[2].strip() not in ("byte", "phase", "loop") \
                or not f[3].strip().isdigit():
            bad.append((n, "a row is path<TAB>function<TAB>byte|phase|loop<TAB>count<TAB>owner lane<TAB>why it stays"))
            continue
        key = (f[0].strip(), f[1].strip(), f[2].strip())
        if key in rows:
            bad.append((n, f"duplicate row {' '.join(key)}"))
        rows[key] = (n, int(f[3]), f[4].strip())
    return rows, bad


def precise_rows(ctx, text):
    """-> the keys of the rows saying PRECISE_ROW when the parent's RQ still matched a call by name alone (the one landing
    that made the match precise), else none."""
    rd = getattr(ctx, "base_read", None)
    t = rd(G + "lint/rules/rq_polls.py") if rd else None
    if t is None or "def calls(" in t:
        return set()
    out = set()
    for raw in (text or "").split("\n"):
        f = raw.split("#", 1)[0].split("\t")
        if len(f) >= 6 and PRECISE_ROW in f[5]:
            out.add((f[0].strip(), f[1].strip(), f[2].strip()))
    return out


def check_tree(ctx):
    out = []
    sc = Scan(ctx)
    out += sc.fails
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    for (p, name, kind), lines in sorted(sc.hits.items()):
        row = rows.get((p, name, kind))
        what = {"byte": "polls the abort byte itself", "phase": "labels a step with phase()",
                "loop": "runs a loop that polls no Step"}[kind]
        if row is None:
            out.append((p, lines[0], f"`{name}` {what} ({len(lines)} site(s), lines {', '.join(map(str, lines[:8]))}); "
                                     f"instead: {INSTEAD}"))
        elif len(lines) > row[1]:
            out.append((p, lines[0], f"`{name}` {what}: {len(lines)} site(s), its POLL-DEBT row allows {row[1]} "
                                     f"(the list only shrinks); instead: {INSTEAD}"))
    for (p, name, kind), (n, count, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
        got = len(sc.hits.get((p, name, kind), ()))
        if got == 0:
            ctx.notes.append(f"RQ: {ledger_loc(at, n)}: {p} `{name}` {kind} ({owner}) has no site left: delete the row")
        elif got < count:
            ctx.notes.append(f"RQ: {ledger_loc(at, n)}: {p} `{name}` {kind} ({owner}) {got} site(s), row {count}: lower it")
    for p, f in sorted(CONTROL - sc.control_seen):
        ctx.notes.append(f"RQ: control-plane stop {p} `{f}` reads no abort byte any more: delete it from CONTROL and the README")
    if rows:
        kinds = {k: sum(c for (_, _, kk), (_, c, _) in rows.items() if kk == k) for k in ("byte", "phase", "loop")}
        ctx.notes.append(f"RQ: POLL-DEBT {len(rows)} rows ({sum(kinds.values())} sites: byte {kinds['byte']}, "
                         f"phase {kinds['phase']}, loop {kinds['loop']})")
    if sc.markers:
        by = {}
        for m in sc.markers:
            by[m[2]] = by.get(m[2], 0) + 1
        ctx.notes.append(f"RQ: {len(sc.markers)} bounded markers: "
                         + ", ".join(f"{c} {by[c]}" for c in CATEGORIES if c in by))
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {(bn, bk) for (bp, bn, bk) in base if (bp, bn, bk) not in rows}
        once = precise_rows(ctx, text)
        for (p, name, kind), (n, count, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            b = base.get((p, name, kind))
            if (p, name, kind) in once:
                continue
            if b is None and (name, kind) not in removed:
                out.append((*at(n), f"{p} `{name}` {kind} ({owner}) added: the list only shrinks (ask cancelpoll::Step)"))
            elif b is not None and count > b[1]:
                out.append((*at(n), f"{p} `{name}` {kind} ({owner}) rose from {b[1]} to {count}: the list only shrinks"))
    return out


def seed_rows(ctx, owner_of, why_of):
    """-> {file name: row text} for every site of the tree (the seed; lint/POLL-DEBT written once by --seed)."""
    sc = Scan(ctx)
    return {debt_file(p, name, kind): f"{p}\t{name}\t{kind}\t{len(lines)}\t{owner_of(p, name)}\t{why_of(p, name, kind)}\n"
            for (p, name, kind), lines in sc.hits.items()}


# the planted failures: a raw poll, a phase label, an unpolled loop over a slice, a Step in a struct, a marker of no
# category; the clean ones: a ticked loop, a fixed-domain loop, a marked loop, a loop polled through its callee
_H = G + "hosts/src/ops/scan/planted.rs"
PLANTED_TOML = (_H, "fn walk(rows: &[u32]) -> Result<(), E> {\n    for r in rows {\n        if cancelpoll::canceled() {\n"
                    "            return Err(E);\n        }\n        visit(*r);\n    }\n    Ok(())\n}\n")
CLEAN_TOML = (_H, "fn walk(rows: &[u32]) -> Result<(), E> {\n    let mut step = cancelpoll::step(\"walk\");\n"
                  "    for r in rows {\n        step.tick()?;\n        visit(*r);\n    }\n"
                  "    for k in 0..MAX_LANES {\n        lane(k);\n    }\n"
                  "    // bounded(\"key-columns: one O(1) pass per key column\")\n    for c in cols {\n        col(c);\n    }\n"
                  "    for p in parts {\n        polled_part(p)?;\n    }\n    Ok(())\n}\n\n"
                  "fn polled_part(p: u32) -> Result<(), E> {\n    let mut step = cancelpoll::step(\"part\");\n"
                  "    step.tick()?;\n    Ok(())\n}\n")
MORE_PLANTED_TOML = [
    (_H, "fn walk(rows: &[u32]) {\n    let _p = cancelpoll::phase(\"walk\");\n    visit(rows);\n}\n"),
    (_H, "fn walk(rows: &[u32]) {\n    while let Some(r) = next(rows) {\n        visit(r);\n    }\n}\n"),
    (_H, "struct Walk {\n    step: cancelpoll::Step,\n}\n"),
    (_H, "fn walk(rows: &[u32], x: &X) {\n    for r in rows {\n        x.part(*r);\n    }\n}\n\n"
         "fn part(p: u32) -> Result<(), E> {\n    let mut step = cancelpoll::step(\"part\");\n    step.tick()?;\n    Ok(())\n}\n"),
    (_H, "fn walk(cols: &[u32]) {\n    // bounded(\"few: a handful\")\n    for c in cols {\n        col(c);\n    }\n}\n"),
    (G + "parallel/src/merge.rs", "fn cancelled(cancel: *const u8) -> bool {\n"
                                  "    unsafe { gravity_abi::ctx::abort_byte_set(cancel) }\n}\n"),
    {"files": [(_H, "fn walk(rows: &[u32]) {\n    for r in rows {\n        visit(*r);\n    }\n}\n"),
               (DEBT + "/gravity.hosts.src.ops.scan.planted.rs@walk@loop",
                f"{_H}\twalk\tloop\t1\tkeyrep\tarea 9\n")],
     "base": {DEBT: ""}, "want": "added: the list only shrinks"},
]
# leader-only polls (LEADER): a check() off the leader, behind a branch, after a `continue`, after `&&` in a header, its
# error dropped; a callee that checks on one path only; the pool's own spelling outside run_led; a marker on a checked loop
_W = G + "hosts/src/ops/window/planted.rs"
_HAS_ONE_PATH = ("fn has(&mut self, r: u32) -> PgResult<bool> {\n    if r == 0 {\n        return Ok(false);\n    }\n"
                 "    cancelpoll::interrupts::check()?;\n    Ok(true)\n}\n")
_HAS_FIRST = ("fn has(&mut self, r: u32) -> PgResult<bool> {\n    cancelpoll::interrupts::check()?;\n    if r == 0 {\n"
              "        return Ok(false);\n    }\n    Ok(true)\n}\n")
_NO_STEP = "runs a loop that polls no Step"
MORE_PLANTED_TOML += [
    {"files": [(_H, "fn walk(rows: &[u32]) -> PgResult<()> {\n    for r in rows {\n"
                    "        cancelpoll::interrupts::check()?;\n        visit(*r);\n    }\n    Ok(())\n}\n")],
     "want": "outside RQ's leader-only hosts (LEADER)"},
    {"files": [(_W, "fn walk(rows: &[u32]) -> PgResult<()> {\n    for r in rows {\n        if *r % 64 == 0 {\n"
                    "            cancelpoll::interrupts::check()?;\n        }\n        visit(*r);\n    }\n    Ok(())\n}\n")],
     "want": _NO_STEP},
    {"files": [(_W, "fn walk(rows: &[u32]) -> PgResult<()> {\n    for r in rows {\n        if *r == 0 {\n"
                    "            continue;\n        }\n        cancelpoll::interrupts::check()?;\n        visit(*r);\n    }\n"
                    "    Ok(())\n}\n")],
     "want": _NO_STEP},
    {"files": [(_W, "fn walk(&mut self, n: u32) -> PgResult<()> {\n    let mut a = 0;\n    while a < n && self.has(a)? {\n"
                    "        a += 1;\n    }\n    Ok(())\n}\n\n" + _HAS_FIRST)],
     "want": _NO_STEP},
    {"files": [(_W, "fn walk(rows: &[u32]) {\n    for r in rows {\n        let _ = cancelpoll::interrupts::check();\n"
                    "        visit(*r);\n    }\n}\n")],
     "want": _NO_STEP},
    {"files": [(_W, "fn walk(&mut self, rows: &[u32]) -> PgResult<()> {\n    for r in rows {\n        self.has(*r)?;\n    }\n"
                    "    Ok(())\n}\n\n" + _HAS_ONE_PATH)],
     "want": _NO_STEP},
    {"files": [(G + "pool/src/lib.rs", "fn drain(&self) -> Result<(), E> {\n    loop {\n"
                                       "        if let Err(e) = (self.ambient.interrupts)() {\n            return Err(stash(e));\n"
                                       "        }\n        self.step();\n    }\n}\n")],
     "want": _NO_STEP},
    {"files": [(_W, "fn walk(rows: &[u32]) -> PgResult<()> {\n"
                    "    // bounded(\"other: every pass checks for interrupts, as PostgreSQL's loop does\")\n"
                    "    for r in rows {\n        cancelpoll::interrupts::check()?;\n        visit(*r);\n    }\n    Ok(())\n}\n")],
     "want": "every pass: delete the marker"},
]
MORE_CLEAN_TOML = [
    (G + "driver/src/entry.rs", "fn run_pipeline_at(b: &Binding) -> Result<()> {\n"
                                "    if gravity_abi::ctx::abort_set(b.abort_flag) {\n        return Err(Error::Cancelled);\n"
                                "    }\n    Ok(())\n}\n"),
    {"files": [(_H, "fn walk(rows: &[u32]) {\n    for r in rows {\n        visit(*r);\n    }\n}\n"),
               (DEBT + "/gravity.hosts.src.ops.scan.planted.rs@walk@loop",
                f"{_H}\twalk\tloop\t1\tkeyrep\tarea 9\n")]},
    (_W, "fn walk(&mut self, rows: &[u32], n: u32) -> PgResult<()> {\n    for r in rows {\n"
         "        cancelpoll::interrupts::check()?;\n        if *r == 0 {\n            continue;\n        }\n        visit(*r);\n"
         "    }\n    let mut a = 0;\n    while self.has(a)? {\n        a += 1;\n    }\n    loop {\n        a += 1;\n"
         "        if !self.has(a)? || a > n {\n            break;\n        }\n    }\n    for r in rows {\n        self.has(*r)?;\n"
         "    }\n    Ok(())\n}\n\n" + _HAS_FIRST),
    (G + "pool/src/lib.rs", "fn run_led(&self) -> Result<(), E> {\n    loop {\n        if self.done() {\n            break;\n"
                            "        }\n        if let Err(e) = (self.ambient.interrupts)() {\n            return Err(stash(e));\n"
                            "        }\n        if self.register() {\n            continue;\n        }\n        park();\n    }\n"
                            "    Ok(())\n}\n"),
]


def _main(argv):
    import argparse
    ap = argparse.ArgumentParser(description="RQ tools: the bounded markers, the POLL-DEBT seed")
    ap.add_argument("--root", default=os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../..")))
    ap.add_argument("--list-bounded", action="store_true")
    a = ap.parse_args(argv)

    class C:
        root = a.root
        notes = []
        base_ledger = None

        def read(self, p):
            try:
                with open(os.path.join(self.root, p), encoding="utf-8", errors="replace") as fh:
                    return fh.read()
            except OSError:
                return None

    sc = Scan(C())
    if a.list_bounded:
        by = {}
        for p, ln, cat, callee, why in sc.markers:
            by[cat] = by.get(cat, 0) + 1
            print(f"{p}:{ln}\t{cat}{':' + callee if callee else ''}\t{why}")
        print("bounded markers: " + (", ".join(f"{c} {by[c]}" for c in CATEGORIES if c in by) or "none")
              + f" ({sum(by.values())})")
    return 0


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
