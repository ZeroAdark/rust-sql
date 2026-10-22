"""RP: process metadata in a code comment. A comment states an invariant, explains why, or cites PostgreSQL's source; it
never names who asked for the code or when (CONSTRUCT-BUNDLE-RULES "Quality rules" 1). Flagged in comment text: a lane
or "main" or "the user" as an actor, a ruling with its date or time, a timestamp (ISO, HH:MMZ, a 2026+ date), a census or
task number, a "constructs #N" reference. D-ids (D1234) and PostgreSQL citations stay.

ON TOUCH: the runner reports the tree's hits as legacy (never a failure, and they keep no LEGACY line alive); a landing
(--base) fails each added or moved line that hits, with no move excuse: strip on touch. While lint/RP-WARN exists those
hits print as notes instead (the warn-only window lanes get to re-preflight; deleting the file turns enforcement on).
"lane" alone is not flagged: Gravity's columns are lanes (a code lane, a pgrc lane), and "census" alone is a diagnostic
counter set."""
import re
from common import G, in_planner, in_src

ID = "RP"
TITLE = "process metadata in a comment (a lane, main or the user as an actor, a ruling date, a timestamp, a census / task / constructs number)"
INSTEAD = "state the invariant or the reason, cite PG's source or the D-id; drop who asked and when"
HOME = []
ON_TOUCH = True
WARN = G + "lint/RP-WARN"   # while this file exists, touched-line hits are notes (the warn-only window before enforcement)


def SCOPE(p):
    return in_src(p) or in_planner(p)


def _alt(names):
    return "(?:" + "|".join(re.escape(n) for n in sorted(names, key=len, reverse=True)) + ")"


# Lanes named as actors. A lane name that is also a technical word (scan, pgrc, numeric, dict, code) is left out of the
# "<x> lane" arm; the "lane <x>" and line-leading arms take only names no comment uses as plain words.
_LANES = {"agg", "aggtrans", "append", "admit", "ctescan", "contract-runtime", "contract-interface", "core-face", "core-rep",
          "drain", "errorhold", "expr", "group", "hashjoin", "initplan", "inputs", "keysem", "lifetime", "lockrows",
          "material", "memoize", "mergecmd", "mergejoin", "modifytable", "nestloop", "normalize", "oracle",
          "producerchain", "quality", "rerun", "rescanaudit", "result", "semantics", "setop", "sort", "spicmp", "srf",
          "structure", "subplan", "topn", "window", "writesubplan"}
_STRICT = {"aggtrans", "ctescan", "contract-runtime", "contract-interface", "core-face", "core-rep", "errorhold", "hashjoin",
           "initplan", "isokinds", "keysem", "lockrows", "mergecmd", "mergejoin", "modifytable", "nestloop", "producerchain",
           "rescanaudit", "semantics", "setop", "spicmp", "srf", "structure", "subplan", "topn", "writesubplan"}
_ENTRY = (_LANES | _STRICT) - {"window", "memoize"}    # PG regress files share these names (window.sql#343 is a statement)
_D = r"(?:20\d\d-)?(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])"
_T = r"\d\d:\d[\dx](?::\d\d)?Z?"

PATTERNS = [
    ("main", re.compile(r"\bmain's\b|\bmain\s+(?:said|says|ruled|rules|decided|decides|asked|approved|wants|ruling|consent|go\b"
                        r"|20\d\d-|" + _D + r"\b|" + _T + r")|\(main\)|\(main,\s*(?:20\d\d|\d\d[-:])|\bper main\b")),
    ("user", re.compile(r"\b(?:the )?user(?:'s)?,?\s+" + _D + r"\b|\buser(?:'s)? (?:ruling|go\b|decision|said|asked|approved)"
                        r"|\(the user[,)]|\bthe user in session\b")),
    ("actor-date", re.compile(r"\b[A-Z][a-z]{2,}(?:'s)?,?\s+" + _D + r"\b")),
    ("lane", re.compile(r"(?:^\W*|\(|\bthe |\bby |[;,] )" + _alt(_LANES) + r" lane\b|\blane " + _alt(_STRICT) + r"\b"
                        r"|(?<!regress )\b" + _alt(_ENTRY) + r"#\d+"
                        r"|^\W*" + _alt(_STRICT) + r"\s+(?:[A-Z][A-Z0-9]*-[A-Z0-9-]+|[A-Z]{4,}|[A-Z]\d+[a-z]?)\b")),
    ("ruling", re.compile(r"\brul(?:ing|ed|es)\b[^.;]{0,60}?(?:\b" + _D + r"\b|\b" + _T + r")"
                          r"|(?:\b" + _D + r"\b|\b" + _T + r")[^.;]{0,40}?\brulings?\b|\bRULED\b|\bruling (?:\d+|\(\d+\)|log)\b")),
    ("timestamp", re.compile(r"\b20\d\d-\d\d-\d\dT\d\d:\d\d|\b\d\d:\d[\dx](?::\d\d)?Z\b|\b20\d{6}T\d{4,6}Z\b"
                             r"|\b202[6-9]-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])\b|\(" + _D + r"\)")),
    ("census", re.compile(r"\bconstructs? census\b|\bcensus (?:task|r\d+\b|20\d\d-|e2e-)|\b[Tt]ask\s*#?\d+\b")),
    ("constructs", re.compile(r"\bconstructs?\s*#\s*\d+")),
]
NEEDS = None


def kinds(text):
    """-> the pattern names that hit one comment text (the selftest and reports use it)."""
    return [k for k, rx in PATTERNS if rx.search(text)]


def check(src, ctx):
    """A comment line hits when a pattern matches in it, or starts in it and runs onto the next comment line (a wrapped
    `(main` + `09-27)`)."""
    out, cm = [], src.comments
    for i, c in enumerate(cm):
        if not c.strip():
            continue
        nxt = cm[i + 1].strip() if i + 1 < len(cm) else ""
        text = c + " " + nxt if nxt else c
        for _, rx in PATTERNS:
            m = rx.search(text)
            if m and m.start() < len(c):
                out.append(i + 1)
                break
    return out


PLANTED = [
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// main's sibling-file layout\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n/// D1035 (main 09-27, direction on bundle 3): every pipeline runs on the leader\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// a wrapped actor (main\n// 09-27): the leader runs it\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n//! The dump knob (the user 09-24): names a directory\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// the fresh-backend floor (Someone, 2026-09-15): one worker\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// D2719 / D2640 (group lane): a key under a user `=`\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// ported onto v5 by lane setop, worklist P6\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// the float folds do (group#293: class_float's risk)\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// aggtrans T6f: the type's default btree comparator\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// ruling 2026-10-04 15:11Z: call counts are free\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// DENY BY DEFAULT (ruling of 09-23): an unlisted builtin\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// the probe keeps it (2026-10-04T20:45:39Z)\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// B3 (07:41Z): the build may spill\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// constructs census task 2\n"),
    (G + "lowerv4/src/ops/sort/planted.rs", "pub fn f() {}\n// a refusal stays where lowering raises it (constructs #21)\n"),
]
CLEAN = [
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() {}\n// D2784: an ORDER BY over a VOLATILE call keeps PG's order (nodeSort.c ExecSort)\n"),
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() {}\n// A code lane's window is its part's dictionary entries; a pgrc lane is loaded sign-extended\n"),
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() {}\n// the zone census over the resident granules (main, InitPlans, arms) and (user code, or a built-in)\n"),
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() {}\n// regress window#343 and matview#113: the build-first twin; PostgresMain's loop\n"),
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() {}\n// datecol >= timestamp '2020-03-01 12:00'; the equal-member ruling keeps PG's value\n"),
    (G + "lowerv4/src/ops/sort/clean.rs", "pub fn f() -> &'static str { \"main's 2026-10-04T15:11Z ruling\" }\n"),
]
