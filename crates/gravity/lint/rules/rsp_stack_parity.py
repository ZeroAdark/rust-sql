"""RSP: PostgreSQL 18.6 checks the stack here, and the port must too (C6, the stack-depth parity check; reports/
BUG-RECURRENCE-20261011.md §3 C6, main 2026-10-11T23:43:04Z). PostgreSQL 18.6 checks its stack depth at 148 places
(check_stack_depth(), and stack_is_too_deep() where an error cannot be raised), each a recursion that the user's input can drive arbitrarily deep (a nested expression, a jsonb document, a tsquery, a
partition tree, a composite type). Without the check a deep enough input runs into the thread's stack end and the
server dies (SIGSEGV) where PostgreSQL raises 54001 'stack depth limit exceeded'. Lint RS covers Gravity's own
recursions by their call graph. RSP covers the rest of the port by PostgreSQL's own list: each of the 148 sites has
its Rust counterpart, and that counterpart keeps its check.

The table, lint/STACK-PARITY (a directory, one file per PostgreSQL C file, named after its path with '/' as '.'): a
header `# PostgreSQL 18.6 <c file>: <n> stack-depth check sites`, then one row per site: C line, C function, Rust
file, Rust function (`name`, or `Type::name` for a method), status, reason. The C side is frozen: PostgreSQL 18.6
does not change, so the 148 rows are its census, re-derivable with lint/reports/stack_parity_census.py where the C
source is at hand (not run by the harness). Status:
- `yes`: the Rust function holds a stack-depth check (`check_stack_depth*`, the lowerer's / decompose's `stack_check`,
  `stack_is_too_deep`). RSP fails when its file or function is gone, or when its body holds no check.
- `n/a` (no recursion in the port, a depth bounded by a constant, a Drop of a tree built under its own check),
  `gravity` (a row executor site: Gravity has none, RS covers its recursions), `unported` (no Rust port of that C
  code): the reason says why. An `unported` row fails once a ported CATALOG.tsv unit lists its C file: name the Rust
  function and its check.
Every failure names the row file to edit, so a lane in any crate can act on it: a function that moved takes its row
along (edit the Rust file / function columns); a function that lost its check gets it back. A header count that
disagrees with its rows, or a total other than 148, fails: rows never drop out. The census checks (the total and
CATALOG.tsv) run where the tree holds CATALOG.tsv, the repository root; elsewhere (the selftest's trees) RSP checks
the rows alone."""
import os
import re
import subprocess

from common import G, Src
from rs_stack import items

ID = "RSP"
TITLE = "a PostgreSQL 18.6 stack-depth check site whose Rust counterpart is gone or holds no stack check"
INSTEAD = "keep PostgreSQL's check in the port (stack_depth::check_stack_depth()? first); a function that moved takes " \
          "its row in lint/STACK-PARITY along"
GLOBAL = True
TABLE = G + "lint/STACK-PARITY"
TOTAL = 148                            # PostgreSQL 18.6's check_stack_depth() / stack_is_too_deep() calls (src/, contrib/)
CATALOG = "CATALOG.tsv"
STATUSES = ("yes", "n/a", "gravity", "unported")
PORTED = ("ported", "benched", "audited", "done")

CHECK = re.compile(r"\b(?:check_stack_depth\w*|stack_check|stack_is_too_deep)\s*(?:::\s*<[^>]*>\s*)?\(")
_HEAD = re.compile(r"^# PostgreSQL 18\.6 (\S+): (\d+) stack-depth check sites$")


_GIT_ENV = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
                                                             "GIT_PREFIX")}


def _text(ctx, path):
    """`path`'s text: the working tree's, else HEAD's when a sparse checkout leaves the file out (the Mac preflight's
    worktree holds the gravity crates, not every backend crate); None when neither has it (the file is gone)."""
    if os.path.isfile(os.path.join(ctx.root, path)):
        return ctx.read(path)
    r = subprocess.run(["git", "-C", ctx.root, "show", f"HEAD:{path}"], capture_output=True, env=_GIT_ENV)
    return r.stdout.decode("utf-8", "replace") if r.returncode == 0 else None


def _fns(ctx, path, cache):
    """-> {label: [body text]} of the functions `path` defines (`name`, `Type::name` for a method), or None when the
    file is gone."""
    if path not in cache:
        text = _text(ctx, path)
        if text is None:
            cache[path] = None
        else:
            src = Src(path, text)
            t = src.joined("code")
            blocks, fns = items(t)
            out = {}
            for name, _line, a, b, at, _params in fns:
                inner = [blk for blk in blocks if blk[0] <= at < blk[1]]
                ty = min(inner, key=lambda blk: blk[1] - blk[0])[2] if inner else None
                label = f"{ty[6:] if ty.startswith('trait ') else ty}::{name}" if ty else name
                out.setdefault(label, []).append(t[a:b])
            cache[path] = out
    return cache[path]


def _ported_files(ctx):
    """-> {C file: unit} of the CATALOG.tsv units a ported status lists, or None without CATALOG.tsv."""
    if not os.path.isfile(os.path.join(ctx.root, CATALOG)):
        return None
    out = {}
    for line in ctx.read(CATALOG).split("\n")[1:]:
        c = line.split("\t")
        if len(c) < 4 or c[1] in ("", "-") or c[2].split(":")[0] not in PORTED:
            continue
        for f in c[1].split(","):
            f = re.sub(r"\(partial\)$", "", f.strip())
            if f:
                out.setdefault(f, c[0])
    return out


def check_tree(ctx):
    out = []
    d = os.path.join(ctx.root, TABLE)
    ported = _ported_files(ctx)
    if not os.path.isdir(d):
        if ported is None:
            return []                  # a tree that is not the repository (the runner's own selftest trees)
        return [(TABLE, 1, f"the stack-depth parity table is gone: restore {TABLE}/ (PostgreSQL 18.6's {TOTAL} "
                           "stack-depth check sites and their Rust counterparts)")]
    cache, total = {}, 0
    for name in sorted(os.listdir(d)):
        if name == "README" or name.startswith(".") or not name.endswith(".tsv"):
            continue
        rf = TABLE + "/" + name
        lines = ctx.read(rf).split("\n")
        m = _HEAD.match(lines[0]) if lines else None
        if not m:
            out.append((rf, 1, "the first line is not '# PostgreSQL 18.6 <c file>: <n> stack-depth check sites'"))
            continue
        cfile, want = m.group(1), int(m.group(2))
        if name != cfile.replace("/", ".") + ".tsv":
            out.append((rf, 1, f"the file of {cfile}'s rows is named {cfile.replace('/', '.')}.tsv"))
        rows, seen = 0, set()
        for n, line in enumerate(lines[1:], 2):
            if not line.strip() or line.startswith("#"):
                continue
            c = line.split("\t")
            if len(c) != 6:
                out.append((rf, n, "a row is: C line, C function, Rust file, Rust function, status, reason (6 tab-separated "
                                   "columns)"))
                continue
            rows += 1
            cline, cfn, rpath, rfn, status, why = c
            site = f"{cfile}:{cline} {cfn}"
            if cline in seen:
                out.append((rf, n, f"C line {cline} is listed twice"))
            seen.add(cline)
            if status not in STATUSES:
                out.append((rf, n, f"status '{status}' is not one of {', '.join(STATUSES)}"))
            elif status == "yes":
                fns = _fns(ctx, rpath, cache)
                if fns is None:
                    out.append((rf, n, f"PostgreSQL 18.6 checks the stack in {site}; its Rust counterpart's file {rpath} is "
                                       f"gone: if `{rfn}` moved, move its row ({rf}:{n}) to the new file"))
                elif rfn not in fns:
                    out.append((rf, n, f"PostgreSQL 18.6 checks the stack in {site}; {rpath} defines no `{rfn}`: if it moved "
                                       f"or was renamed, edit its row ({rf}:{n}) to the new file / name"))
                elif not any(CHECK.search(body) for body in fns[rfn]):
                    out.append((rpath, 1, f"`{rfn}` holds no stack-depth check where PostgreSQL 18.6 calls "
                                          f"check_stack_depth() ({site}): a deep enough input overflows the thread's "
                                          "stack (SIGSEGV) instead of raising 54001. Call stack_depth::check_stack_depth()? "
                                          "first (check_stack_depth_or_panic() where no PgResult returns); if the check "
                                          f"moved to another function, move the row ({rf}:{n}) to it"))
            elif not why.strip():
                out.append((rf, n, f"a '{status}' row says why ({site}): the reason column is empty"))
            elif status == "unported" and ported is not None and cfile in ported:
                out.append((rf, n, f"{cfile} is ported now (CATALOG.tsv unit {ported[cfile]}): name the Rust function of "
                                   f"{cfn}, give it PostgreSQL's stack check, and mark the row 'yes'"))
        if rows != want:
            out.append((rf, 1, f"the header counts {want} sites of {cfile} and the file lists {rows}: PostgreSQL 18.6's "
                               "census is frozen, a row never drops out"))
        total += rows
    if ported is not None and total != TOTAL:
        out.append((TABLE, 1, f"{total} sites listed, PostgreSQL 18.6 has {TOTAL}: a row never drops out (re-derive with "
                              "lint/reports/stack_parity_census.py)"))
    ctx.notes.append(f"RSP: {total} PostgreSQL 18.6 stack-depth check sites checked")
    return out


# planted: a 'yes' row whose Rust function no longer checks the stack; clean: an 'n/a' row with its reason
PLANTED_TOML = (TABLE + "/src.backend.utils.adt.planted.c.tsv",
                "# PostgreSQL 18.6 src/backend/utils/adt/planted.c: 1 stack-depth check sites\n"
                "12\tplanted_walk\tcrates/backend/utils/adt/planted/src/lib.rs\tplanted_walk\tyes\t\n")
CLEAN_TOML = (TABLE + "/src.backend.utils.adt.clean.c.tsv",
              "# PostgreSQL 18.6 src/backend/utils/adt/clean.c: 1 stack-depth check sites\n"
              "12\tclean_free\t-\t-\tn/a\tthe Rust port is a loop over the state stack, no recursion\n")
MORE_PLANTED_TOML = (
    (TABLE + "/src.backend.utils.adt.counts.c.tsv",      # a header that disagrees with its rows
     "# PostgreSQL 18.6 src/backend/utils/adt/counts.c: 2 stack-depth check sites\n"
     "12\tclean_free\t-\t-\tn/a\tbounded by a constant\n"),
    (TABLE + "/src.backend.utils.adt.why.c.tsv",         # an unported row with no reason
     "# PostgreSQL 18.6 src/backend/utils/adt/why.c: 1 stack-depth check sites\n"
     "12\twhy_walk\t-\t-\tunported\t\n"),
)
