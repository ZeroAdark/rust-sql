"""lowerv4 census mirror of props/merge.rs `merge_pull` (V4R-143, D478): whether a pipeline is a pull-form MergeAppend arm,
whose source kind graph.rs maps to runs.mergeappend. Split from graphprops.py (its budget); M3-MERGE."""
import re


def merge_pull_arm(pipes, p):
    """lowerv4 graph.rs `source_kind` (V4R-143, D478): pipeline `p` is an arm of a MergeAppend in the pull form
    (props/merge.rs `merge_pull`): under the reader's LIMIT, at most 32 arms, each a heap index scan whose ops before
    its feed are filters and projections, every merge key a by-value word or a text (V4R-458), every other column one or
    a varlena."""
    if p.idx is None or p.end[0] != "breaker" or "feed" not in p.op_names():
        return False
    slot = p.end[1]
    k = pipes.index(p)
    feeds = lambda q: q.end[0] == "breaker" and q.end[1] == slot and any(n == "feed" and re.match(r"feed slot=%d\b" % slot, b) for n, b, _ in q.ops)
    j = next((j for j in range(k + 1, len(pipes)) if any(re.match(r"append merge slot=%d\b" % slot, b) for _, b, _ in pipes[j].ops)), None)
    if j is None:
        return False
    r = pipes[j]
    at = next(i for i, (_, b, _) in enumerate(r.ops) if b.startswith("append merge"))
    m = re.search(r"children=(\d+)", r.ops[at][1])
    n = int(m.group(1)) if m else 0
    arms = [q for q in reversed(pipes[:j]) if feeds(q)][:n][::-1]
    if len(arms) != n or p not in arms or n > 32:
        return False
    cap = False
    for name, body, _ in r.ops[at + 1:]:
        if name == "project":
            continue
        cap = name == "limit" and "with-ties" not in body and not body.startswith("limit count=all")
        break
    def arm(q):
        s = q.source or ""
        f = q.op_names().index("feed")
        return s.split(" ", 1)[0] in ("index", "index-only") and " am=heap" in s and all(x in ("filter", "project") for x in q.op_names()[:f])
    last = arms[-1]
    km = re.search(r" keys=(\S+)", r.ops[at][1])
    ks = [int(re.match(r"\d+", x).group()) for x in km.group(1).split(",")] if km else []
    # V4R-458: a text / varchar key (sort.rs dump_keys: kind Fp128, image Text printed as nothing; bpchar is `/Bpchar`)
    texts = {int(re.match(r"\d+", x).group()) for x in km.group(1).split(",") if re.match(r"\d+[-+]n?:Fp128(@\d+)?$", x)} if km else set()
    outs = last.outs[-1] if last.outs else []
    # a key by value or a text key; another column by value or a varlena (V4R-305: runs/merge_pull_copy.rs keeps its copy)
    byval = bool(outs) and all(re.match(r"w\d+:val$", rep) or ((c + 1 not in ks or c + 1 in texts) and rep.startswith("vl:")) for c, rep in enumerate(outs))
    fits = len(outs) <= 64 and len(ks) <= 32 and len(outs) + len(ks) + sum(rep.startswith("vl:") for rep in outs) <= 127
    return cap and all(arm(q) for q in arms) and byval and fits
