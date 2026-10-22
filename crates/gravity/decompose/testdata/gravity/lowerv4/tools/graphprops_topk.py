"""graphprops.py's mirrors of M3-TOPS' props (V4R-126), kept out of graphprops.py's budget:
- topk_wide: props/topk.rs topk_wide (road topk.fetch's shape);
- top_reused: graph.rs top_reused (D295, M3 L0-g), which end_need reads for a top over drained groups.
"""
import re

# props/topk.rs FETCH_COLS / FETCH_REFS: road topk.fetch's wide row
FETCH_COLS, FETCH_REFS = 32, 8


def topk_wide(p, body, inp):
    """props/topk.rs topk_wide (V4R-126 / V4R-169, road topk.fetch): a top over rows (topk / zonewalk, no ties) straight over
    a pgrc range, every sorted-row entry a bare column of that scan the fetch site reads (a 2 / 4 / 8-byte word, a code or
    a varlena lane), every key such a word, more than FETCH_COLS entries or more than FETCH_REFS not by value."""
    s = p.source or ""
    m = re.match(r"range .*\brti=(\d+) .*\bam=pgrc\b", s)
    if not re.search(r" form=(topk|zonewalk) ", body) or not m or inp is None:
        return False
    kinds = {}
    for ln in p.src_lines:
        if ln.startswith("lanes"):
            for t in ln.split()[1:]:
                lm = re.match(r"!?Attr\((-?\d+)\):([a-z]+)(\d*)", t)
                if lm:
                    kinds[int(lm.group(1))] = (lm.group(2), int(lm.group(3) or 0))
    lanes = []
    for t in inp:
        e = t.kids[0] if t.head == "te" and t.kids else None
        if e is None or e.head != "var" or e.extra[0] != "v" + m.group(1) or e.extra[1] < 1 or e.extra[1] not in kinds:
            return False
        kind, width = kinds[e.extra[1]]
        if kind == "byval" and width not in (2, 4, 8):
            return False
        lanes.append(kind)
    keys = re.search(r"keys=(\S+)", body)
    for km in re.finditer(r"(\d+)[-+]", keys.group(1) if keys else ""):
        r = int(km.group(1))
        if r < 1 or r > len(lanes) or lanes[r - 1] != "byval":
            return False
    refs = sum(k != "byval" for k in lanes)
    return len(lanes) > FETCH_COLS or refs > FETCH_REFS


def group_row_sort(p):
    """graph.rs drains_grouping (D1021): only filters, gates and projections run before pipeline `p`'s sort (the ops a
    top over groups resolves); past any other op its rows are not the group's and the top orders rows (runs.topk_rows)."""
    s = next((i for i, (n, _, _) in enumerate(p.ops) if n == "sort"), len(p.ops))
    return all(n in ("filter", "gate", "project") for n, _, _ in p.ops[:s])


def top_reused(w, k):
    """graph.rs top_reused (D295, M3 L0-g) over graphprops' Walk `w`: a top over drained groups whose group slot or own
    slot is produced again up to the top's reader keeps its rows by value (runs.topk_rows)."""
    ps = w.g.pipes
    p = ps[k]
    m = re.match(r"breaker slot=(\d+)", p.source or "")
    if p.end[0] != "breaker" or not m:
        return False
    slot, own = int(m.group(1)), p.end[1]
    r = next((j for j in range(k + 1, len(ps)) if re.match(r"breaker slot=%d\b" % own, ps[j].source or "")), None)
    if r is None:
        return False
    group = w.life_of(p, slot)
    group = k if group is None else group
    again = lambda s, frm: any(ps[q].end[0] == "breaker" and ps[q].end[1] == s for q in range(frm + 1, r + 1))
    return again(slot, group) or again(own, k)
