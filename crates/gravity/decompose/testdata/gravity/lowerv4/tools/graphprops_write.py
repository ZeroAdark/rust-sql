"""graphprops.py's mirror of the write line's props, split from graphprops.py for its budget (V4R-204, M4-L0): props/ops.rs
Op::Write's multi_target (V4R-5), route and row_trigger (V4R-204), and props/write.rs triggers' write.transition (L0-j),
read from the write line and the plan fixture's `-- rel` / `-- trigext` / `-- part` header lines."""
import os
import re
import sys

TRIG_CMD = {"INSERT": "i", "UPDATE": "u", "DELETE": "d", "MERGE": "iud"}  # decompose trig_facts::cmd_bits


def G(w):
    """graphprops.py's module (the Walk's own: under __main__ too)."""
    return sys.modules[type(w).__module__]


def write(w, body):
    """Walk's write line: the props and kinds props/ops.rs's Op::Write arm and props/write.rs triggers add."""
    g, fid = G(w), w.g.fid
    # V4R-5 (props.rs): several result relations (tableoid= is printed for targets > 1), Member::Targets (a
    # route whose root is an inheritance parent, not a partitioned table: its relkind from the fixture's header),
    # or Member::CrossPartition (the route's ` cross-partition`: a DELETE + routed INSERT, one target or more)
    m = re.search(r" route=rti\d+:(\d+)", body)
    root = int(m.group(1)) if m else None
    if " tableoid=" in body or " cross-partition" in body or (m and g.relkind(fid, root) not in ("p", None)):
        w.P.add("multi_target")
    # V4R-227 (props/write.rs marks): a write op with its own row marks (the write line's ` marks=`) needs write.lock
    if " marks=" in body:
        w.K.add("write.lock")
    wm = re.match(r"write (INSERT|UPDATE|DELETE|MERGE) rel=(\d+)", body)
    if not wm:
        return
    tag, rel = wm.group(1), int(wm.group(2))
    # V4R-204 (props/ops.rs, M4 Decision 2): an INSERT / MERGE routed through a partitioned parent (Member::Route)
    # is route; a BEFORE / INSTEAD OF ROW INSERT trigger on the target, or no trigext line, is row_trigger
    if tag in ("INSERT", "MERGE") and m and g.relkind(fid, root) == "p":
        w.P.add("route")
    t = trigext(g, fid, rel)
    if t is None or "i" in t[0] + t[2]:
        w.P.add("row_trigger")
    # props/write.rs triggers (V4-PLAN decision 1): a row op whose written relation, or its route root, has a trigger on a
    # command it runs (BEFORE / AFTER / INSTEAD OF row, statement or transition; the fixture's `-- trigext` line), or no
    # trigext line (unknown facts), needs write.transition. L0-j: ON CONFLICT DO UPDATE runs UPDATE's; a cross-partition
    # UPDATE runs DELETE's and INSERT's, on every leaf under the root too (no `-- part` line under it: unknown, firing)
    cross = " cross-partition" in body
    cmds = TRIG_CMD[tag] + ("u" if " on_conflict=update" in body else "") + ("di" if cross else "")
    hit = fires(t, cmds) or bool(m and fires(trigext(g, fid, root), cmds))
    if not hit and cross:
        leaves = part_leaves(g, fid, root) if m else None
        hit = leaves is None or any(trig and fires(trigext(g, fid, leaf), cmds) for leaf, trig in leaves)
    if hit:
        w.K.add("write.transition")


def part_leaves(g, fid, root):
    """corpus.rs partition_leaves: the leaves under `root` by the fixture's `-- part <parent> <child> trig= rules=` lines
    (a partitioned child is walked, not a leaf) as (relid, has_triggers), or None: no line under the root (unknown)."""
    p = g.plan_path(fid)
    if not fid or not os.path.exists(p):
        return None
    parts = []
    with open(p, errors="replace") as fh:
        for line in fh:
            if not line.startswith("-- "):
                break
            pm = re.match(r"^-- part (\d+) (\d+) trig=(\w)", line)
            if pm:
                parts.append((int(pm.group(1)), int(pm.group(2)), pm.group(3) == "t"))
    if not any(pa == root for pa, _, _ in parts):
        return None
    out = []

    def under(parent):
        for pa, child, trig in parts:
            if pa == parent:
                under(child) if g.relkind(fid, child) == "p" else out.append((child, trig))
    under(root)
    return out


def trigext(g, fid, relid):
    """The relation's `-- trigext <relid> before= after= instead= stmt= trans= ...` header line as its five command sets, or
    None: no line (TrigExt::UNKNOWN)."""
    p = g.plan_path(fid)
    if not fid or not os.path.exists(p):
        return None
    with open(p, errors="replace") as fh:
        for line in fh:
            if not line.startswith("-- "):
                break
            m = re.match(r"^-- trigext %d before=(\S+) after=(\S+) instead=(\S+) stmt=(\S+) trans=(\S+)" % relid, line)
            if m:
                return m.groups()
    return None


def fires(t, cmds):
    """props/write.rs triggers' `fires`: a row, statement or transition trigger on one of the commands, or unknown facts."""
    return t is None or any(c in "".join(t) for c in cmds)
