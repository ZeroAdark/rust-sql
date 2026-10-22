"""The detector's findings out of cargo's JSON messages (collect.sh clippy): one per (file, line, col, kind, item).

kind is the config entry's reason tag (`alloclint:<type|method|growth|macro>`, clippy.toml); a finding inside std's macro body
(vec!, format!) lands on its call site, and is dropped there when the call site's macro finding already names it."""
import json
import os
import re


def _scope_files():
    """scope.tsv's 4th column: the only files of that crate in scope (comma-separated, relative to the crate dir); main
    2026-10-14T19:42:45Z (iv): the pgrc COPY files of crates/backend/commands/copy, not the whole crate"""
    out = {}
    p = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scope.tsv")
    if os.path.exists(p):
        for ln in open(p):
            if ln.startswith("#") or not ln.strip():
                continue
            c = ln.rstrip("\n").split("\t")
            if len(c) > 3 and c[3].strip():
                out[c[0].rstrip("/")] = {x.strip() for x in c[3].split(",") if x.strip()}
    return out


SCOPE_FILES = _scope_files()


def crate_dir(f, crates):
    """the linted crate a repo-relative file belongs to: the longest crate dir whose src/ holds it (crates/gravity/backend
    and its nested crates/gravity/backend/interp are two crates)"""
    best = None
    for c in crates:
        if f.startswith(c + "/src/") and (best is None or len(c) > len(best)):
            best = c
    if best in SCOPE_FILES and f[len(best) + 1:] not in SCOPE_FILES[best]:
        return None   # a crate scoped by file (scope.tsv's 4th column): this file is not one of them
    return best


CANON = [(re.compile(r"^alloc::"), "std::"), (re.compile(r"^core::(iter|clone)::"), r"std::\1::")]


def canon(item):
    """one name per item whatever spelling clippy reported (a no_std crate's finding names the alloc:: / core:: path)"""
    for rx, rep in CANON:
        item = rx.sub(rep, item)
    return item


def parse(path, crates):
    """crates: one crate dir or a list of them (round 2: every Gravity crate in one clippy run)"""
    crates = [crates] if isinstance(crates, str) else list(crates)
    inc = lambda f: crate_dir(f, crates) is not None  # noqa: E731
    found, from_macro = [], set()
    for ln in open(path, errors="replace"):
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if d.get("reason") != "compiler-message":
            continue
        m = d["message"]
        code = (m.get("code") or {}).get("code") or ""
        if not code.startswith("clippy::disallowed_"):
            continue
        sp = next((x for x in m.get("spans", []) if x.get("is_primary")), None)
        if not sp:
            continue
        walked = False
        while not inc(sp["file_name"]) and sp.get("expansion"):
            sp = sp["expansion"]["span"]
            walked = True
        if not inc(sp["file_name"]):
            continue
        item = re.findall(r"`([^`]+)`", m.get("message", ""))
        item = canon(item[0]) if item else "?"
        kind = ""
        for ch in m.get("children", []):
            mm = re.search(r"alloclint:(\w+)", ch.get("message", ""))
            if mm:
                kind = mm.group(1)
        if not kind:
            kind = {"clippy::disallowed_types": "type", "clippy::disallowed_macros": "macro"}.get(code, "method")
        found.append((sp["file_name"], sp["line_start"], sp["column_start"], kind, item))
        if walked:
            from_macro.add((sp["file_name"], sp["line_start"], item))
    macro_at = {(f, l) for f, l, c, k, it in found if k == "macro"}
    return sorted({x for x in found if not ((x[0], x[1], x[4]) in from_macro and (x[0], x[1]) in macro_at)})


# TYPE-MENTION (USER RULING via main 2026-10-14T23:20:10Z, TD-197 amendment): a `type` finding names a container type (a
# field, a signature, a turbofish, a `use`) and allocates nothing by itself. On a line with no allocation finding (method /
# growth / macro) it is a TYPE-MENTION: counted per crate in its own column, never a ledger row, outside NEW and the debt
# total. On a line that also holds an allocation finding it stays debt beside that line's rows. The test reads the raw
# findings (before homes / Cold tags), so the S1 leg (clippy only) and the full leg classify every finding the same way.
ALLOC_KINDS = ("method", "growth", "macro")


def alloc_lines(found):
    """(file, line) of every allocation finding: a type finding on any other line is a TYPE-MENTION"""
    return {(x[0], x[1]) for x in found if x[3] in ALLOC_KINDS}


def is_mention(x, lines):
    return x[3] == "type" and (x[0], x[1]) not in lines


def compiler_errors(path):
    """cargo's error messages (a build that failed reports no findings; the ratchet must not read that as a clean tree)."""
    out = []
    for ln in open(path, errors="replace"):
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if d.get("reason") == "compiler-message" and d["message"].get("level") == "error":
            out.append(d["message"].get("rendered") or d["message"].get("message", ""))
    return out


# REFCOUNT-CLONE (USER GO via main 2026-10-15T11:46:31Z, TD-197 amendment 6): a `Clone::clone` finding whose RESOLVED receiver
# is an Rc / Arc / rc::Weak / sync::Weak only bumps a count and allocates nothing. The type is the compiler's: clippy's
# clone_on_ref_ptr (collect.sh forces it beside the disallowed_* lints) fires on a `.clone()` method call exactly when the
# adjusted receiver, its references peeled, is one of those four types (`(*rc).clone()` and `T::clone(&*rc)` reach T's clone:
# no lint, the finding stays a site; `Arc::clone(&a)` names its type in a path the lint does not check: it stays a site too).
# On a line whose findings are all such clones each is a REFCOUNT-CLONE: counted per crate in its own column, never a ledger
# row, outside NEW and the debt total. A line with any other finding (a type named beside it included) keeps every finding as
# debt. Like a TYPE-MENTION it covers nothing in the IR leg (crosscheck.py). Rc::new / Arc::new / make_mut and Vec / Box / String /
# map clones are untouched (other items, or no clone_on_ref_ptr span). The match is by the call's END, not by containment: the
# lint's span is the whole call expression, so a Vec clone nested in an Arc clone's receiver (`m[k.clone()].clone()`) lies inside
# it; only the finding whose `clone()` closes the span (its ident + 7 columns = the span's end) is the Arc clone. MEASURED at
# trunk 472fd94b246: 190 lint spans, each closed by exactly one Clone::clone finding (of 1,995), none nested.
CLONE_ITEMS = ("std::clone::Clone::clone",)


def refptr_spans(path):
    """{file: {(line_end, col_end), ...}}: where each clone_on_ref_ptr message's primary span (the whole call expression,
    receiver included, so it may start on an earlier line) ends"""
    out = {}
    for ln in open(path, errors="replace"):
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if d.get("reason") != "compiler-message":
            continue
        m = d["message"]
        if ((m.get("code") or {}).get("code") or "") != "clippy::clone_on_ref_ptr":
            continue
        sp = next((x for x in m.get("spans", []) if x.get("is_primary")), None)
        if sp:
            out.setdefault(sp["file_name"], set()).add((sp["line_end"], sp["column_end"]))
    return out


CLONE_CALL = len("clone()")


def _closes(ends, f, line, col):
    """the Clone::clone finding at (line, col) (its method ident) closes a clone_on_ref_ptr span: `clone()` ends it"""
    return (line, col + CLONE_CALL) in ends.get(f, ())


def refcount_findings(found, ends):
    """the findings that are REFCOUNT-CLONEs: a Clone::clone whose `clone()` closes a clone_on_ref_ptr span, on a line whose
    every finding is one too"""
    by_line = {}
    for x in found:
        by_line.setdefault((x[0], x[1]), []).append(x)
    rc = lambda x: x[4] in CLONE_ITEMS and _closes(ends, x[0], x[1], x[2])  # noqa: E731
    return {x for xs in by_line.values() if all(rc(y) for y in xs) for x in xs}


# REFCOUNT-CLONE-INLINE (USER RULING via memacct 2026-10-15T18:5xZ, TD-197 amendment 7): an Rc / Arc / Weak clone whose type
# the compiler resolved (it closes a clone_on_ref_ptr span) allocates nothing wherever it stands, alone on its line or beside
# other findings. The ones alone on their line keep the REFCOUNT-CLONE column; the rest are REFCOUNT-CLONE-INLINE, their own
# column, never a row, outside NEW and the debt total. The line's OTHER findings stay debt (a Vec clone beside it, an
# Arc::new), and a type named on a line whose only allocation finding was such a clone is a TYPE-MENTION (the line allocates
# nothing). Only clippy's resolution counts: an Option<Rc | Arc> clone fires no clone_on_ref_ptr and stays debt here.
def refcount_clones(found, ends):
    """every Clone::clone finding whose `clone()` closes a clone_on_ref_ptr span, alone on its line or not"""
    return {x for x in found if x[4] in CLONE_ITEMS and _closes(ends, x[0], x[1], x[2])}


# NOALLOC (TD-197 LINT-FP, USER RULING via memacct 2026-10-15T20:3xZ: a false positive is fixed in the lint, never left as
# debt): clippy names the trait method, not the receiver, so these items fire on receivers that allocate nothing (Extend on
# an ArenaVec: allocator_api2 Vec<T, ArenaRef>, the arena's home; derive(Clone) of u32 / bool / Range<u32> / Option<Copy>
# fields; to_owned on an i32; Clone on an Option<Arc>). The IR leg (crosscheck.py, opt-level 0: every call explicit, its
# callee monomorphized) proves a finding's statement makes no global allocation; run.sh full writes those findings to
# x/noalloc.tsv, ratchet.py write keeps them per crate in ALLOCLINT-DEBT/<crate>/.noalloc keyed on the line's normalized
# text, and the clippy leg reads a finding so listed as NOALLOC (a named class: no row, not debt) while the line's text is
# unchanged. An edited line is debt again until a full run re-derives it; the S3-slow alloclint-ir slot re-derives the
# table on every last-green and reports a row the IR no longer proves.
NOALLOC_ITEMS = ("std::clone::Clone::clone", "std::borrow::ToOwned::to_owned", "std::iter::Extend::extend",
                 "std::iter::Iterator::collect")


def line_key(text):
    """a source line's key in .noalloc: its text with whitespace collapsed"""
    import hashlib
    return hashlib.sha1(" ".join(text.split()).encode()).hexdigest()[:12]


# COLD-TEXT by TYPE (crosscheck.py; memacct 2026-10-15T20:4xZ): a Cold-tagged Box::new whose payload the IR shows is a PgError
# (an error payload on the error path) is COLD-TEXT, kept in .noalloc with class COLD-TEXT (the same text key and re-proof)
BOX_NEW = "std::boxed::Box::new"
RE_BOX_NEW = re.compile(r"^alloc::boxed::Box<T(,\s*A)?>::new$")
ERR_PAYLOAD_TITLES = ("new<types_error::pg_error::PgError>", "new<types_error::pg_error::PgError, alloc::alloc::Global>")
