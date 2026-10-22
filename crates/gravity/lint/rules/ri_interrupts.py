"""RI: an interrupt check outside the interrupt facade (C6 (d), reports/BUG-RECURRENCE-20261011.md §3 C6). The home is
`cancelpoll::interrupts` (crates/_support/cancelpoll/src/interrupts.rs): `check()` (C's CHECK_FOR_INTERRUPTS),
`check_abort()` (the abort class), `pending()` (an interrupt the backend can take now, read only), `pending_abort()` (the
abort class of it) and `flagged()` (INTERRUPT_PENDING alone, read only). A Gravity check for
a pending cancel, statement_timeout or terminate used to be spelled five ways, and each interrupt fix added a site of
its own (D2909, D3552, D3575, D3610, D3764, D3811, D3837, D3887). RI reads every production Gravity source
(`crates/gravity/<crate>/src/**`) and finds, by construct, charged to the innermost function:
- a call of PostgreSQL's interrupt seams (`postgres_seams::check_for_interrupts::call` / `is_installed`, the same for
  `check_for_abort_interrupts`): ask `check()` / `check_abort()`;
- a read of the backend's interrupt globals (`InterruptPending()`, `InterruptsCanBeProcessed()`, `AbortPending()`):
  ask `pending()` / `pending_abort()` / `flagged()`;
- an install of an interrupt probe or check into the compiler (`set_interrupt_probe(..)`, `set_interrupt_check(..)`),
  whatever it installs: gravity_tpde's stop points, the compile wait and gravity_backend's `take_interrupt` call the
  facade themselves (C6 (d) I3 deleted both setters and the words they filled).
A site not moved yet needs a row in lint/INTERRUPT-DEBT (a directory, one row per file): path, function, owner lane and
why it stays; an unlisted one FAILS. On a landing (--base) a row the parent lacks fails (the list only shrinks), unless
it replaces a row of the same function the landing removes (a move), or the parent has no ledger yet. A row whose site
is gone is a note (delete it). The abort byte's own test (gravity_abi `ctx::abort_set`) and the kernels' generated poll
(lowerv4 frame/cancel.rs) are not checks of this kind: they read a byte, and a set byte calls the facade."""
import re

from common import G, ledger_loc, Src, generated, in_src, walk_sources
from rl_lifetime import functions

ID = "RI"
TITLE = "an interrupt check outside the interrupt facade"
INSTEAD = "cancelpoll::interrupts::check() / check_abort() / pending() / pending_abort() / flagged() " \
          "(crates/_support/cancelpoll/src/interrupts.rs); " \
          "not yet moved: a lint/INTERRUPT-DEBT row"
GLOBAL = True
DEBT = G + "lint/INTERRUPT-DEBT"

SITE = re.compile(r"\bcheck_for_(?:abort_)?interrupts\s*::\s*(?:call|is_installed)\b"
                  r"|\b(?:InterruptPending|InterruptsCanBeProcessed|AbortPending)\s*\("
                  r"|(?<!fn\s)\bset_interrupt_(?:probe|check)\s*\(")
_NEEDS = re.compile(r"check_for_(?:abort_)?interrupts|InterruptPending|InterruptsCanBeProcessed|AbortPending"
                    r"|set_interrupt_(?:probe|check)")


def sites(src):
    """-> {(function, line)}: the functions of `src` holding an interrupt check of their own (module doc)."""
    out = set()
    code = src.joined("code")
    fns = functions(src)
    for m in SITE.finditer(code):
        inner = [(n, ln, b - a) for n, ln, (a, b) in fns if a <= m.start() < b]
        if inner:
            n, ln, _ = min(inner, key=lambda x: x[2])
            out.add((n, ln))
        else:
            out.add(("<item>", code.count("\n", 0, m.start()) + 1))
    return out


def debt_file(path, name):
    """A row's file name: <path below crates/gravity, '/' as '.'>@<function>."""
    return path[len(G):].replace("/", ".") + "@" + name


def parse_debt(text):
    """-> ({(path, function): (line, owner)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 4 or not all(x.strip() for x in f[:4]):
            bad.append((n, "a row is path<TAB>function<TAB>owner lane<TAB>why it stays"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip())
    return rows, bad


def scan(ctx):
    """-> {(path, function): line} over the production Gravity sources."""
    hits = {}
    for p in walk_sources(ctx.root):
        if not in_src(p):
            continue
        t = ctx.read(p)
        if t is None or generated(t) or not _NEEDS.search(t):
            continue
        for name, line in sites(Src(p, t)):
            hits.setdefault((p, name), line)
    return hits


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    hits = scan(ctx)
    for (p, name), n in sorted(hits.items()):
        if (p, name) not in rows:
            out.append((p, n, f"`{name}` checks for interrupts itself; instead: {INSTEAD}"))
    ctx.notes += [f"RI: {ledger_loc(at, n)}: {p} `{name}` ({owner}) holds no interrupt check of its own: delete the row"
                  for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, name) not in hits]
    if rows:
        ctx.notes.append(f"RI: INTERRUPT-DEBT {len(rows)} rows")
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bn for bp, bn in base if (bp, bn) not in rows}
        for (p, name), (n, owner) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, name) not in base and name not in removed:
                out.append((*at(n), f"{p} `{name}` ({owner}) added: the list only shrinks (ask cancelpoll::interrupts)"))
    return out


# the planted failure calls PostgreSQL's seam itself; the clean one asks the facade
PLANTED_TOML = (G + "hosts/src/ops/planted.rs",
                "fn batch() -> PgResult<()> {\n    postgres_seams::check_for_interrupts::call()?;\n    Ok(())\n}\n")
CLEAN_TOML = (G + "hosts/src/ops/clean.rs",
              "fn batch() -> PgResult<()> {\n    // not postgres_seams::check_for_interrupts::call() itself\n"
              "    cancelpoll::interrupts::check()?;\n    Ok(())\n}\n")
MORE_PLANTED_TOML = [
    (G + "lowerv4/src/planted.rs",            # the backend's globals read
     "pub fn stop() -> bool {\n    init_small::globals::InterruptPending() && init_small::globals::InterruptsCanBeProcessed()\n}\n"),
    (G + "exec/src/planted.rs",               # a probe of its own installed
     "fn compile() {\n    gravity_backend::gravity_tpde::set_interrupt_probe(my_probe);\n}\n"),
]
MORE_PLANTED_TOML.append(
    (G + "exec/src/planted_facade.rs",        # even the facade's own function, installed (the compiler calls it itself)
     "fn compile() {\n    gravity_backend::cache::set_interrupt_check(cancelpoll::interrupts::check);\n}\n"))
MORE_CLEAN_TOML = [
    (G + "backend/tpde/src/clean.rs",         # a stop point asks the facade
     "pub fn interrupt_pending() -> bool {\n    cancelpoll::interrupts::pending()\n}\n"),
]
