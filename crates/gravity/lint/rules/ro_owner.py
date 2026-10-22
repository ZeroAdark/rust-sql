"""RO: memory bytes moved, or a use / reservation / depot entry kept, outside the owned-charge home (C8 "owned, measured
charges", the user-approved step-2 spec; design lanes/memlead/C8-DESIGN.md §5). The home is memledger's `Charge`
(crates/_support/memledger/src/): every byte that moves between accounts moves with its owner through `Charge::transfer`,
and a part use, a cache reservation or a depot entry holds a `Charge`. RO reads every production .rs file under crates/
(test code is out: tests plant and witness through the API) and finds:
- a call: a direct ledger byte move (`memledger::<f>(` for the functions in `MOVES`, a bare name a file imports from
  memledger, the driver's `budget::` glob re-export of memledger), charged to the innermost function;
- a holder: a part-cache dictionary reservation (pgrc_read's `zeroed_words(`), charged to the innermost function; a
  bare `acct` field (`acct: u32` / `acct: Acct`) in the memory files, charged to its struct.
Each site needs a row in lint/PIN-OWNER-DEBT (a directory, one row per file): path, site (the function, `Type::fn` inside
an impl, or the struct), owner lane, the C8 area move that removes it (move1 .. move7), the count of constructs the site
holds, and what it moves. A site with no row FAILS, and so does a site holding more constructs than its row's count. On a
landing (--base) a row the parent lacks, or a count above the parent's, fails (the list only shrinks), unless the row
replaces one of the same site name the landing removes (a certified move). A row whose site is gone is a note.

Four holders are checked at their DEFINITIONS, with no row and for every caller (C8 area move 2c; until it they were found
at each call, `.use_part(` / `.use_part_in(` and `DEPOT.put(` / `DEPOT.take(`, owned or not):
- a part use: `PartUse` is built in exactly one place, in its own file (openpart.rs), and that struct literal takes
  `Charge::use_of(`: no part use exists without an owner. A second literal, one with no `Charge::use_of(`, or one in
  another file FAILS.
- a worker-set depot entry: `DEPOT.put(` / `DEPOT.take(` are called only in the depot's file (driver slots.rs), and a
  function that puts a set passes its kept part uses to the depot first (`kept_uses_to(Owner::Cache(Holder::Parked)`).
  A put with no such move before it, or a put / take in another file, FAILS. A take moves nothing: a kept cursor outlives
  the statement that borrows the set, so its use stays the depot's, and a cursor the statement opens is the statement's
  by the first rule. RO only fences the take to the depot's file: that a taken set's uses have the right owner is guarded
  by e2e v5/c8_parked_set_uses and by checker 1's worker-set and portal sites (memledger owner_end), not by this rule. A
  parked set holds no region: its regions park in the region free list, checked as the third holder below.
- a parked region (C8 area move 3; main 2026-10-12T13:48:02Z (1)): the region free list stores a region only after
  its charge has passed to the depot's holder: in driver park.rs a function that pushes a region into the free
  list's classes (`q[..].push_back(` / `push_front(`) calls `.parked()` before the push, and RegionBuf::parked
  (driver mem/region.rs) moves its charge to `Owner::Cache(Holder::Parked)`. A push with no `.parked()` before it, or
  a `parked` that moves the charge anywhere else (or is gone), FAILS.
- an mcx mapped block (C8 area move 7, C1; main 2026-10-12T15:15:33Z (3) + 17:02:28Z, memlead 15:18:45Z + 17:00:54Z): a
  memory context's mapped block is raw allocator memory with no header for a `Charge`, so it is charged through the
  home's `memledger::charge::door_mapped` / `door_moving` / `door_remapped` (the map's context owner, its cell and its
  account, which the home remembers by the block's address for its unmap and resize), which mcx heap.rs's `door` /
  `door_move` / `door_remap` call and nothing else does. A `door_mapped(` / `door_moving(` / `door_remapped(` call outside
  mcx heap.rs, a heap.rs `door` that does not call `door_mapped(`, a heap.rs `door_move` that does not call
  `door_moving(`, or a heap.rs `door_remap` that does not call `door_remapped(`, FAILS. A remap (`os::remap(` /
  `libc::mremap(`, outside the `os` layer) with no `door_move(` before it in its function FAILS: the remap may free the
  old range first, and a block another thread maps there must not be taken for the moving one. Every mcx mapping is charged: in
  crates/_support/mcx/src/ a function that maps (`os::map(` / `os::remap(`, or a raw `libc::mmap(` / `libc::mremap(`
  outside heap.rs's `os` layer, whose callers are checked) calls the door (`door(` / `door_remap(`) after the mapping;
  one with no door after it FAILS. And the door is the only way: a door-2 `Charge` (`Door::Mapped`) in mcx's sources
  FAILS, as a direct ledger move there needs a row like anywhere."""
import os
import re

from common import G, ledger_loc, Src, generated, in_any
from rl_lifetime import functions

ID = "RO"
TITLE = "memory moved or kept outside the owned-charge home"
INSTEAD = "a memledger Charge (Charge::reserve / open / use_of, commit / uncommit, the one Charge::transfer); not yet " \
          "moved: a lint/PIN-OWNER-DEBT row naming the C8 area move that removes it"
GLOBAL = True
DEBT = G + "lint/PIN-OWNER-DEBT"
HOME = ["crates/_support/memledger/src/"]
ROOTS = ("crates/",)
SKIP = ("crates/postgres-18.6-reference/", "crates/context/", G + "lint/")
MOVES = ("charge", "discharge", "charge_to", "discharge_from", "charge_kind", "discharge_kind", "pool_charge", "transfer",
         "transfer_kind", "give_back", "take_back", "enter", "set_mapped", "count_kind")
STEPS = {"move1", "move2", "move3", "move4", "move5", "move6", "move7"}
# the files where a bare `acct` field is a holder (a memory owner's identity kept as a bare account)
ACCT_FILES = ["crates/backend/access/pgrcolumnar/pgrc_read/src/", G + "driver/src/mem/", G + "driver/src/slots.rs",
              G + "driver/src/park.rs"]

_M = "|".join(MOVES)
CALL = re.compile(r"\b(?:memledger|budget)::(" + _M + r")\s*\(")
USE_LIST = re.compile(r"\buse\s+(?:memledger|(?:gravity_driver|crate)::mem::budget|super::budget)::\{([^}]*)\}")
USE_ONE = re.compile(r"\buse\s+(?:memledger|(?:gravity_driver|crate)::mem::budget|super::budget)::(\w+)\s*;")
# the two holders checked at their definitions (see the module doc)
PARTUSE_LIT = re.compile(r"\bPartUse\s*\{")
SELF_LIT = re.compile(r"\bSelf\s*\{")
USE_OF = re.compile(r"\bCharge::use_of\s*\(")
DEPOT_CALL = re.compile(r"\bDEPOT\s*\.\s*(put|take)\s*\(")
PARKED_MOVE = re.compile(r"\bkept_uses_to\s*\(\s*(?:memledger::)?Owner::Cache\s*\(\s*(?:memledger::)?Holder::Parked\s*\)")
_NOT_LITERAL = re.compile(r"(?:->|\b(?:struct|impl|for|let|enum|trait|union|mut|ref))\s*$")
ZEROED = re.compile(r"(?<![\w:])zeroed_words\s*\(")
ACCT_FIELD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?acct\s*:\s*(?:u32|(?:memledger::)?Acct)\s*,", re.M)
STRUCT = re.compile(r"\bstruct\s+(\w+)")
IMPL = re.compile(r"^[ \t]*(?:unsafe[ \t]+)?impl\b(?:\s*<[^{;]*?>)?\s+(?:[\w:]+(?:<[^{;]*?>)?\s+for\s+)?([\w:]+)(?:<[^{;]*?>)?[^{;]*\{", re.M)
_NEEDS = re.compile(r"memledger|budget::|PartUse|DEPOT|zeroed_words|\bacct\s*:|door_mapped")
PARTUSE_FILE = "crates/backend/access/pgrcolumnar/pgrc_read/src/openpart.rs"
DEPOT_FILE = G + "driver/src/slots.rs"
# the third holder checked at its definitions: a parked region (see the module doc)
REGION_FILE = G + "driver/src/mem/region.rs"
PARK_FILE = G + "driver/src/park.rs"
FREE_PUSH = re.compile(r"\bq\s*\[[^\]]*\]\s*\.\s*push_(?:back|front)\s*\(")
PARKED_CALL = re.compile(r"\.parked\s*\(\s*\)")
TO_PARKED = re.compile(r"\bOwner::Cache\s*\(\s*(?:memledger::)?Holder::Parked\s*\)")
ZEROED_SCOPE = "crates/backend/access/pgrcolumnar/pgrc_read/src/"
# the fourth holder checked at its definitions: an mcx mapped block (see the module doc)
MCX_SRC = "crates/_support/mcx/src/"
MCX_DOOR_FILE = MCX_SRC + "heap.rs"
DOOR_MAPPED = re.compile(r"\bdoor_(?:(?:re)?mapped|moving)\s*\(")
DOOR_HOME = {"door": "door_mapped", "door_move": "door_moving", "door_remap": "door_remapped"}
MCX_REMAP = re.compile(r"\bos::remap\s*\(|\blibc::mremap\s*\(")
MCX_MOVE = re.compile(r"(?<!\w)door_move\s*\(")
MCX_MAP = re.compile(r"\bos::(?:map|remap)\s*\(|\blibc::(?:mmap|mremap)\s*\(")
MCX_DOOR = re.compile(r"(?<!\w)door(?:_remap)?\s*\(")
DOOR2 = re.compile(r"\bDoor::Mapped\b")


def is_test_path(p):
    base = os.path.basename(p)
    return ("/tests/" in p or "/benches/" in p or "/examples/" in p or base in ("tests.rs", "test.rs")
            or base.endswith("_tests.rs") or base.endswith("_test.rs"))


def sources(ctx):
    """Every production .rs file under crates/ (tests, the C reference, the lint itself and the home left out)."""
    out = []
    for r in ROOTS:
        top = os.path.join(ctx.root, r)
        for dp, dn, fn in os.walk(top):
            dn[:] = sorted(d for d in dn if d not in ("target", "tests", "benches", "examples") and not d.startswith("."))
            for f in sorted(fn):
                if f.endswith(".rs"):
                    p = os.path.relpath(os.path.join(dp, f), ctx.root).replace(os.sep, "/")
                    if not in_any(p, list(SKIP) + HOME) and not is_test_path(p):
                        out.append(p)
    return out


def impls(t):
    """-> [(type, (body start, body end))] of the impl blocks in code text `t`."""
    out = []
    for m in IMPL.finditer(t):
        i, depth = m.end() - 1, 0
        for k in range(i, len(t)):
            if t[k] == "{":
                depth += 1
            elif t[k] == "}":
                depth -= 1
                if depth == 0:
                    out.append((m.group(1).split("::")[-1], (i, k)))
                    break
    return out


def sites(src):
    """-> {site: (line, count)} of the direct moves and holders in `src` (see the module doc)."""
    t = src.joined("code")
    names = set()
    for m in USE_LIST.finditer(t):
        names |= {n.strip().split(" as ")[0] for n in m.group(1).split(",")} & set(MOVES)
    names |= {m.group(1) for m in USE_ONE.finditer(t)} & set(MOVES)
    rx = [CALL]
    if names:
        rx.append(re.compile(r"(?<![\w:.])(" + "|".join(sorted(names)) + r")\s*\("))
    if src.path.startswith(ZEROED_SCOPE):
        rx.append(ZEROED)
    fns, blocks = functions(src), impls(t)
    out = {}
    for r in rx:
        for m in r.finditer(t):
            if re.search(r"\bfn\s+$", t[max(0, m.start() - 8):m.start()]):
                continue
            inner = [(n, ln, b - a, a) for n, ln, (a, b) in fns if a <= m.start() < b]
            if not inner:
                continue
            n, ln, _, a = min(inner, key=lambda x: x[2])
            owners = [(ty, e - s) for ty, (s, e) in blocks if s <= a < e]
            site = (min(owners, key=lambda x: x[1])[0] + "::" + n) if owners else n
            line, c = out.get(site, (ln, 0))
            out[site] = (line, c + 1)
    if in_any(src.path, ACCT_FILES):
        for m in ACCT_FIELD.finditer(t):
            st = [s for s in STRUCT.finditer(t, 0, m.start())]
            if st:
                site = st[-1].group(1)
                line, c = out.get(site, (t.count("\n", 0, st[-1].start()) + 1, 0))
                out[site] = (line, c + 1)
    return out


def _close_brace(t, i):
    """The index of the `}` that closes the `{` at t[i], else -1."""
    depth = 0
    for k in range(i, len(t)):
        if t[k] == "{":
            depth += 1
        elif t[k] == "}":
            depth -= 1
            if depth == 0:
                return k
    return -1


def literals(t, rx, lo=0, hi=None):
    """-> [(start, body)] of the struct literals `rx` (`Name {`) opens in t[lo:hi]: not a definition (`struct` / `impl` /
    `for Name {`), a return type (`-> Name {`) or a pattern (`let Name { .. } =`, a match arm's `Name { .. } =>`)."""
    out = []
    for m in rx.finditer(t, lo, len(t) if hi is None else hi):
        if _NOT_LITERAL.search(t[max(0, m.start() - 32):m.start()]):
            continue
        k = _close_brace(t, m.end() - 1)
        if k < 0:
            continue
        rest = t[k + 1:k + 40].lstrip(") \t\n")
        if rest.startswith("=>") or (rest.startswith("=") and not rest.startswith("==")):
            continue
        out.append((m.start(), t[m.end():k]))
    return out


def defined(src):
    """The holders checked at their definitions (module doc) -> [(path, line, note)]."""
    p, t, out = src.path, src.joined("code"), []

    def line(i):
        return t.count("\n", 0, i) + 1

    lits = literals(t, PARTUSE_LIT)
    if p == PARTUSE_FILE:
        for ty, (a, b) in impls(t):
            if ty == "PartUse":
                lits += literals(t, SELF_LIT, a, b)
        lits.sort()
        st = re.search(r"\bstruct\s+PartUse\b", t)
        if st and not lits:
            out.append((p, line(st.start()), "`PartUse` has no constructor RO can read: a part use is built in one place, a "
                                             "struct literal that takes `Charge::use_of(owner, release)`"))
        for i, body in lits[:1]:
            if not USE_OF.search(body):
                out.append((p, line(i), "`PartUse` is built with no owner: its one constructor takes "
                                        "`Charge::use_of(owner, release)`, so no part use exists without an owner"))
        for i, _ in lits[1:]:
            out.append((p, line(i), "a second `PartUse` constructor: a part use is built in one place, with its owner "
                                    "(`Charge::use_of`); take it through `use_part` / `use_part_in` and `transfer` it"))
    else:
        for i, _ in lits:
            out.append((p, line(i), "a `PartUse` built outside its file (pgrc_read openpart.rs): take it through `use_part` / "
                                    "`use_part_in`, which give it its owner"))
    calls = list(DEPOT_CALL.finditer(t))
    if calls and p != DEPOT_FILE:
        for m in calls:
            out.append((p, line(m.start()), f"`DEPOT.{m.group(1)}(` outside the depot's file (driver slots.rs): a worker set "
                                            f"parks and leaves through `Node::release_workers` / `Node::provision`, which "
                                            f"pass its kept part uses to the depot"))
    elif calls:
        fns = functions(src)
        for m in calls:
            if m.group(1) != "put":
                continue
            inner = [(b - a, a) for _, _, (a, b) in fns if a <= m.start() < b]
            a = min(inner)[1] if inner else 0
            if not PARKED_MOVE.search(t[a:m.start()]):
                out.append((p, line(m.start()), "a worker set is put into the depot with its kept part uses still its last "
                                                "backend's: pass them first, `kept_uses_to(Owner::Cache(Holder::Parked), ..)` "
                                                "on each of its sources"))
    if p == PARK_FILE:
        fns = functions(src)
        for m in FREE_PUSH.finditer(t):
            inner = [(b - a, a) for _, _, (a, b) in fns if a <= m.start() < b]
            a = min(inner)[1] if inner else 0
            if not PARKED_CALL.search(t[a:m.start()]):
                out.append((p, line(m.start()), "a region is pushed into the free list with its charge still its statement's: "
                                                "call `.parked()` first (its charge passes to the depot's holder)"))
    if p == REGION_FILE:
        body = [t[a:b] for n, _, (a, b) in functions(src) if n == "parked"]
        if not body or not any(TO_PARKED.search(x) for x in body):
            st = re.search(r"\bstruct\s+RegionBuf\b", t)
            out.append((p, line(st.start()) if st else 1, "`RegionBuf::parked` must move the region's charge to "
                                                          "`Owner::Cache(Holder::Parked)` (the free list's holder)"))
    if p != MCX_DOOR_FILE:
        for m in DOOR_MAPPED.finditer(t):
            out.append((p, line(m.start()), "`door_mapped(` / `door_moving(` / `door_remapped(` outside mcx heap.rs: they are a "
                                            "memory context's mapped block's door (mcx `heap::door` / `door_move` / `door_remap`); "
                                            "any other memory holds a `Charge`"))
    else:
        fns = functions(src)
        for name, home in DOOR_HOME.items():
            doors = [(ln, t[a:b]) for n, ln, (a, b) in fns if n == name]
            if name in ("door_move", "door_remap") and not doors:
                continue
            if not any(re.search(r"\b" + home + r"\s*\(", x) for _, x in doors):
                out.append((p, doors[0][0] if doors else 1, f"mcx heap.rs's `{name}` must charge a mapped block through the "
                                                            f"home's `memledger::charge::{home}` (the owner the home remembers for "
                                                            f"the block)"))
    if p.startswith(MCX_SRC):
        fns = functions(src)
        for m in MCX_MAP.finditer(t):
            inner = [(b - a, a, b, n) for n, _, (a, b) in fns if a <= m.start() < b]
            if not inner:
                continue
            _, a, b, n = min(inner)
            if p == MCX_DOOR_FILE and m.group(0).startswith("libc") and n in ("map", "remap"):
                continue
            if MCX_REMAP.match(m.group(0)) and not MCX_MOVE.search(t[a:m.start()]):
                out.append((p, line(m.start()), "an mcx remap with no `door_move(` before it: the remap may free the old range "
                                                "first, and a block mapped there before `door_remap` must not be taken for this one"))
            if not MCX_DOOR.search(t[m.end():b]):
                out.append((p, line(m.start()), "an mcx mapping with no charge: call heap.rs's `door(` / `door_remap(` (the "
                                                "home's `door_mapped` / `door_remapped`) after it, so the block is its owner's"))
        for m in DOOR2.finditer(t):
            out.append((p, line(m.start()), "a door-2 `Charge` in mcx: a context's mapped block is charged through heap.rs's "
                                            "`door` (the home's `door_mapped`) only"))
    return out


def file_name(path, site):
    """The row's file in the registry directory (LEDGER-DIRS): the path with crates/ dropped, '/' as '.', '@' and the site
    with '::' as '.'."""
    return path[len("crates/"):].replace("/", ".") + "@" + site.replace("::", ".")


def parse_debt(text):
    """-> ({(path, site): (line, owner, step, count)}, [(line, problem)])."""
    rows, bad = {}, []
    for n, raw in enumerate(text.split("\n"), 1):
        s = raw.split("#", 1)[0].rstrip()
        if not s.strip():
            continue
        f = s.split("\t")
        if len(f) < 6 or not all(x.strip() for x in f[:6]):
            bad.append((n, "a row is path<TAB>site<TAB>owner lane<TAB>move<TAB>count<TAB>what it moves or keeps"))
            continue
        step = f[3].strip()
        if step not in STEPS:
            bad.append((n, f"move {step!r} is not a C8 area move ({', '.join(sorted(STEPS))})"))
            continue
        if not f[4].strip().isdigit():
            bad.append((n, f"count {f[4].strip()!r} is not a number"))
            continue
        key = (f[0].strip(), f[1].strip())
        if key in rows:
            bad.append((n, f"duplicate row {key[0]} {key[1]}"))
        rows[key] = (n, f[2].strip(), step, int(f[4].strip()))
    return rows, bad


def scan(ctx):
    """-> ({(path, site): (line, count)}, [(path, line, note)]) over every production file: the sites that need a row, and
    the findings at the definitions."""
    hits, found = {}, []
    for p in sources(ctx):
        t = ctx.read(p)
        if t is None or generated(t) or (not _NEEDS.search(t) and p not in (PARK_FILE, REGION_FILE) and not p.startswith(MCX_SRC)):
            continue
        src = Src(p, t)
        for site, (line, c) in sites(src).items():
            hits[(p, site)] = (line, c)
        found += defined(src)
    return hits, found


def check_tree(ctx):
    out = []
    text, at = ctx.ledger(DEBT)
    rows, bad = parse_debt(text) if text is not None else ({}, [])
    out += [(*at(n), note) for n, note in bad]
    hits, found = scan(ctx)
    out += found
    for (p, site), (n, c) in sorted(hits.items()):
        if (p, site) not in rows:
            out.append((p, n, f"`{site}` moves memory or keeps a use, reservation or depot entry outside the owned-charge "
                              f"home ({c}x); instead: {INSTEAD}"))
        elif c > rows[(p, site)][3]:
            out.append((p, n, f"`{site}` holds {c} direct moves / holders, its PIN-OWNER-DEBT row allows "
                              f"{rows[(p, site)][3]}; instead: {INSTEAD}"))
    stale = [f"{ledger_loc(at, n)}: {p} `{site}` ({owner}) is no longer found: delete the row"
             for (p, site), (n, owner, _, _) in sorted(rows.items(), key=lambda kv: kv[1][0]) if (p, site) not in hits]
    ctx.notes += [f"RO: {s}" for s in stale]
    base_text = ctx.base_ledger(DEBT) if ctx.base_ledger else None
    if base_text is not None:
        base, _ = parse_debt(base_text)
        removed = {bs for bp, bs in base if (bp, bs) not in rows}
        for (p, site), (n, owner, _, c) in sorted(rows.items(), key=lambda kv: kv[1][0]):
            if (p, site) not in base and site not in removed:
                out.append((*at(n), f"{p} `{site}` ({owner}) added: the list only shrinks (move it onto a Charge)"))
            elif (p, site) in base and c > base[(p, site)][3]:
                out.append((*at(n), f"{p} `{site}` ({owner}) count {c} > {base[(p, site)][3]}: the list only shrinks"))
    return out


_ROW = DEBT + "/backend.access.pgrcolumnar.pgrc_read.src.partcache.rs@charge"
_OPENPART = ("pub struct PartUse {\n    part: Arc<OpenPart>,\n    charge: memledger::Charge,\n}\nimpl OpenPart {\n"
             "    pub fn use_part_in(self: &Arc<Self>, token: u64) -> PartUse {\n"
             "        PartUse { part: Arc::clone(self), charge: memledger::Charge::use_of(owner, release) }\n    }\n}\n")
_SLOTS = ("impl Node {\n    pub(crate) fn release_workers(&mut self) {\n        for w in self.workers.iter_mut() {\n"
          "            w.inner.for_each_host(&mut |h| {\n                if let HostRef::Source(s) = h {\n"
          "                    s.kept_uses_to(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);\n"
          "                }\n            });\n        }\n%s"
          "        DEPOT.put(key, core::mem::take(&mut self.workers));\n    }\n"
          "    pub fn provision(&mut self) {\n        if let Some(slots) = DEPOT.take(key) {\n            self.workers = slots;\n"
          "        }\n    }\n}\n")
_PARK = ("impl RegionDepot {\n    pub fn put(&self, mut buf: RegionBuf) {\n        buf.uncommit();\n%s"
         "        let mut classes = self.classes.lock().unwrap();\n        classes.q[c].push_back((stamp, buf));\n    }\n}\n")
_HEAP = ("pub(crate) fn door(ptr: usize, bytes: usize, on: bool) {\n    memledger::charge::door_mapped(ptr, os::round(bytes) as u64, on);\n}\n"
         "pub(crate) fn door_remap(old: usize, new: usize, a: usize, b: usize) {\n"
         "    memledger::charge::door_remapped(old, new, a as u64, b as u64);\n}\n"
         "impl Heap {\n    fn map(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {\n"
         "        let p = os::map(layout.size()).ok_or(AllocError)?;\n%s        Ok(NonNull::slice_from_raw_parts(p, layout.size()))\n"
         "    }\n}\nmod os {\n    pub fn map(size: usize) -> Option<NonNull<u8>> {\n"
         "        let p = unsafe { libc::mmap(core::ptr::null_mut(), size, PROT, FLAGS, -1, 0) };\n        NonNull::new(p.cast())\n"
         "    }\n}\n")
_HEAP_MOVE = ("pub(crate) fn door_move(ptr: usize, on: bool) {\n    memledger::charge::door_moving(ptr, on);\n}\n"
              "impl Heap {\n    unsafe fn resize(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {\n"
              "%s        let Some(p) = os::remap(ptr, old.size(), new.size()) else {\n            return Err(AllocError);\n        };\n"
              "        door_remap(ptr.as_ptr() as usize, p.as_ptr() as usize, old.size(), new.size());\n"
              "        Ok(NonNull::slice_from_raw_parts(p, new.size()))\n    }\n}\n")
_MOVE = "        door_move(ptr.as_ptr() as usize, true);\n"
_ASET = ("pub(super) unsafe fn alloc(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {\n"
         "    let base = libc::mmap(core::ptr::null_mut(), total, PROT, FLAGS, -1, 0);\n%s    Ok(slice(base))\n}\n")
_REGION = ("pub struct RegionBuf {\n    charge: memledger::Charge,\n}\nimpl RegionBuf {\n    pub(crate) fn parked(&mut self) {\n"
           "        self.move_to(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);\n    }\n}\n")
# the planted failures: a direct transfer with no row; a bare imported move; a bare acct field in a memory file; a count
# above its row; a row the parent lacks; and at the two definitions (C8 area move 2c): a second PartUse constructor with no
# owner (a struct literal, and `Self { .. }` in its impl), the one constructor with no owner, a PartUse built in another
# file, a depot put with no move of the set's kept uses before it (none at all, and one that comes after the put), a depot
# put / take outside the depot's file, a region pushed into the free list with no `.parked()` before it, a `parked` that
# moves the region's charge to another owner; and an mcx mapped block (C8 area move 7, C1): a `door_mapped(` call outside mcx
# heap.rs, a heap.rs `door` that does not call it, a heap.rs `door_remap` that books the remap as an unmap + map (the
# resizing thread's owner) instead of `door_remapped(`, an mcx mapping with no door after it (heap.rs's Heap::map, aset.rs's
# guarded chunk), a door-2 `Charge` in mcx
PLANTED_TOML = ("crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
                "pub fn charge(n: u64) {\n    memledger::transfer(memledger::current(), LEDGER, n, false);\n}\n")
CLEAN_TOML = ("crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
              "pub fn charge(n: u64) {\n    let _c = memledger::Charge::open(OWNER, Release::Evict, Kind::Region, Door::Heap, n);\n"
              "    let _e = memledger::charge::enter_as(OWNER);\n}\n")
MORE_PLANTED_TOML = [(G + "hosts/src/worker_arm.rs", "use memledger::{current, enter};\nfn arm(a: Acct) {\n    let _g = enter(a);\n}\n"),
                     (G + "driver/src/mem/region.rs", "pub struct RegionBuf {\n    ptr: *mut u8,\n    acct: u32,\n}\n"),
                     {"files": [(PARTUSE_FILE, _OPENPART + "impl PartUse {\n    pub fn bare(part: Arc<OpenPart>) -> PartUse {\n"
                                               "        PartUse { part, charge: memledger::Charge::NONE }\n    }\n}\n")],
                      "want": "a second `PartUse` constructor"},
                     {"files": [(PARTUSE_FILE, _OPENPART + "impl PartUse {\n    pub fn bare(part: Arc<OpenPart>) -> Self {\n"
                                               "        Self { part, charge: memledger::Charge::NONE }\n    }\n}\n")],
                      "want": "a second `PartUse` constructor"},
                     {"files": [(PARTUSE_FILE, _OPENPART.replace("memledger::Charge::use_of(owner, release)", "memledger::Charge::NONE"))],
                      "want": "built with no owner"},
                     {"files": [("crates/backend/access/pgrcolumnar/pgrc_read/src/cursor.rs",
                                 "fn open(part: Arc<OpenPart>) -> StreamCursor {\n    let part_use = PartUse { part, charge: c };\n}\n")],
                      "want": "built outside its file"},
                     {"files": [(DEPOT_FILE, "impl Node {\n    pub(crate) fn release_workers(&mut self) {\n"
                                             "        DEPOT.put(key, core::mem::take(&mut self.workers));\n    }\n}\n")],
                      "want": "kept part uses still its last"},
                     {"files": [(DEPOT_FILE, "impl Node {\n    pub(crate) fn release_workers(&mut self) {\n"
                                             "        DEPOT.put(key, core::mem::take(&mut self.workers));\n"
                                             "        s.kept_uses_to(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);\n    }\n}\n")],
                      "want": "kept part uses still its last"},
                     {"files": [(G + "driver/src/park.rs", "fn stash(slots: Vec<WorkerSlot>) {\n    crate::slots::DEPOT.put(key, slots);\n}\n")],
                      "want": "outside the depot's file"},
                     {"files": [(PARK_FILE, _PARK % "")], "want": "pushed into the free list"},
                     {"files": [(REGION_FILE, _REGION.replace("memledger::Owner::Cache(memledger::Holder::Parked)", "owner"))],
                      "want": "`RegionBuf::parked` must"},
                     {"files": [(G + "exec/src/runner/execute.rs", "fn grab() {\n    let s = gravity_driver::DEPOT.take(key);\n}\n")],
                      "want": "outside the depot's file"},
                     {"files": [(G + "hosts/src/ops/scan/pgrc/granule_source.rs",
                                 "fn hold(n: u64) {\n    memledger::charge::door_mapped(n, true);\n}\n")],
                      "want": "`door_remapped(` outside mcx heap.rs"},
                     {"files": [(G + "driver/src/mem/region.rs",
                                 "fn grow(a: usize, b: usize) {\n    memledger::charge::door_remapped(a, b, 1, 2);\n}\n")],
                      "want": "`door_remapped(` outside mcx heap.rs"},
                     {"files": [(MCX_DOOR_FILE, _HEAP.replace("memledger::charge::door_mapped(ptr, os::round(bytes) as u64, on)",
                                                              "let _ = (ptr, bytes, on)") % "        door(p, layout.size(), true);\n")],
                      "want": "`door` must charge a mapped block through"},
                     {"files": [(MCX_DOOR_FILE, _HEAP.replace("memledger::charge::door_remapped(old, new, a as u64, b as u64)",
                                                              "door(old, a, false); door(new, b, true)") % "        door(p, layout.size(), true);\n")],
                      "want": "`door_remap` must charge a mapped block through"},
                     {"files": [(MCX_DOOR_FILE, _HEAP % "")], "want": "an mcx mapping with no charge"},
                     {"files": [(MCX_DOOR_FILE, _HEAP % "        door(p, layout.size(), true);\n" + _HEAP_MOVE % "")],
                      "want": "an mcx remap with no `door_move(` before it"},
                     {"files": [(MCX_DOOR_FILE, _HEAP % "        door(p, layout.size(), true);\n"
                                 + _HEAP_MOVE.replace("memledger::charge::door_moving(ptr, on)", "let _ = (ptr, on)") % _MOVE)],
                      "want": "`door_move` must charge a mapped block through"},
                     {"files": [(G + "driver/src/mem/region.rs", "fn hold(a: usize) {\n    memledger::charge::door_moving(a, true);\n}\n")],
                      "want": "`door_moving(` / `door_remapped(` outside mcx heap.rs"},
                     {"files": [(MCX_SRC + "aset.rs", _ASET % "")], "want": "an mcx mapping with no charge"},
                     {"files": [(MCX_SRC + "aset.rs", _ASET % "    crate::heap::door(base as usize, total, true);\n"
                                                              "    let _c = memledger::Charge::open(o, r, memledger::Kind::Mapped, "
                                                              "memledger::charge::Door::Mapped, total as u64);\n")],
                      "want": "a door-2 `Charge` in mcx"},
                     {"files": [("crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
                                 "pub fn charge(n: u64) {\n    memledger::transfer(a, b, n, false);\n    memledger::transfer(b, a, n, false);\n}\n"),
                                (_ROW, "crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs\tcharge\tmembugs\tmove1\t1\tthe census's door-1 transfer\n")],
                      "want": "allows 1"},
                     {"files": [("crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
                                 "pub fn charge(n: u64) {\n    memledger::transfer(a, b, n, false);\n}\n"),
                                (_ROW, "crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs\tcharge\tmembugs\tmove1\t1\tthe census's door-1 transfer\n")],
                      "base": {DEBT: ""}, "want": "added"}]
MORE_CLEAN_TOML = [{"files": [("crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs",
                               "pub fn charge(n: u64) {\n    memledger::transfer(a, b, n, false);\n}\n"
                               "#[cfg(test)]\nmod tests {\n    fn t() { memledger::transfer(a, b, 1, false); }\n}\n"),
                              (_ROW, "crates/backend/access/pgrcolumnar/pgrc_read/src/partcache.rs\tcharge\tmembugs\tmove1\t1\tthe census's door-1 transfer\n")]},
                   (G + "driver/tests/planted.rs", "fn t() {\n    memledger::transfer(a, b, 1, false);\n}\n"),
                   # the two definitions as they stand, and their callers: a part use taken and kept anywhere needs no row
                   (PARTUSE_FILE, _OPENPART + "impl Drop for PartUse {\n    fn drop(&mut self) {\n        let PartUse { part, .. } = self;\n    }\n}\n"),
                   (DEPOT_FILE, _SLOTS % ""),
                   (PARK_FILE, _PARK % "        buf.parked();\n"),
                   (REGION_FILE, _REGION),
                   (MCX_DOOR_FILE, _HEAP % "        door(p, layout.size(), true);\n" + _HEAP_MOVE % _MOVE),
                   (MCX_SRC + "aset.rs", _ASET % "    crate::heap::door(base as usize, total, true);\n"),
                   (G + "hosts/src/ops/scan/pgrc/granule_source.rs",
                    "impl RelParts {\n    fn part(&self, p: usize) {\n        let _ = self.uses[p].set(part.use_part_in(self.token));\n    }\n}\n"),
                   ("crates/_support/memledger/src/charge.rs", "pub fn transfer(&mut self) {\n    post(a1, -c, 0, k, -c);\n    memledger::transfer(a, b, 1, false);\n}\n")]
