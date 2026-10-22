#!/usr/bin/env python3
"""alloclint cross-check, call graph, phase and inventory over EVERY Gravity crate (lane alloclint round 2, 2026-10-14;
reports/ALLOCLINT-GRAVITY-20261014.md sections 2-4; round 1, lowerv4 only: reports/ALLOCLINT-LOWERV4-20261014.md).

    crosscheck.py --root REPO --ll OUT/ir --clippy OUT/clippy.json --out DIR [--crate DIR ...] [--homes homes.tsv]
                  [--roots roots.tsv] [--paths paths.tsv] [--sizes size_class.tsv]
    crosscheck.py selftest        the statement / proof window rules (D4672) on a fixture

Reads every linted crate's LLVM IR (opt-level 0, line tables; collect.sh ir: one module per codegen unit per crate) into ONE
whole-program view: symbols resolve across modules (a generic instantiated in another crate is one function, classified
once; a cross-crate call is an edge), debug metadata and private globals stay per module. It never trusts the source lint:
from the IR alone it finds every call, in a function whose debug-info file is a linted crate's, to a callee that allocates
through the global allocator (`__rust_alloc` / `_zeroed` / `__rust_realloc`, reached through any chain of non-crate functions
defined in the union, or an external std / alloc API named in EXT_ALLOC), without passing a home (HOME by name: mcx, the
program arena, the driver's mem, memledger, the prototype's gravity_fn::arena; HOME_FILE by inline chain; homes.tsv by the
function the site sits in). Each such call is an IR SITE (file:line of the innermost linted frame of its debug location).
A site with no clippy finding in its statement is a MISS; a function with IR sites and no finding at all is a FUNCTION MISS.

Phase: one call graph (direct calls, escaping function pointers, vtables a function builds, statics it reads, vtable calls
resolved by slot offset and the method name on the call's lines) walked from the RUNTIME roots (functions handed to
generated code or the driver: host! fns, extern "C" fns whose address is taken, the HOST_MARK hand-off spellings; every impl
method of the run-time traits RUNTIME_TRAITS; roots.tsv RUNTIME) and the LOWERING roots (roots.tsv LOWERING); roots.tsv
STOP-RUNTIME / STOP-LOWERING are functions a walk does not enter (compiling on the run path is plan work). Writes DIR/
{INVENTORY.tsv, counts.tsv, ir_sites.tsv, misses.tsv, fn_misses.tsv, home_sites.tsv, dep_sites.tsv, roots.tsv, ...,
summary.txt}.
"""
import argparse
import collections
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import findings as findings_mod  # noqa: E402
import homes as homes_mod  # noqa: E402

RE_DEF = re.compile(r'^(define|declare)\b.*?@("[^"]+"|[-\w$.]+)\(')
RE_CALL = re.compile(r'\b(call|invoke)\b.*?([@%](?:"[^"]+"|[-\w$.]+))\(')
RE_DBG = re.compile(r'!dbg !(\d+)')
RE_SYM = re.compile(r'@("[^"]+"|[-\w$.]+)')
RE_MD = re.compile(r'^!(\d+) = (?:distinct )?!(DILocation|DISubprogram|DILexicalBlockFile|DILexicalBlock|DIFile|DINamespace)\((.*)\)\s*$')
RE_FIELD = re.compile(r'(\w+): ("(?:[^"\\]|\\.)*"|!\d+|[^,]+)')
RE_GEP = re.compile(r'^\s*(%[-\w$.]+) = getelementptr inbounds (?:nuw )?i8, ptr (%[-\w$.]+), i64 (\d+)')
RE_LOAD = re.compile(r'^\s*(%[-\w$.]+) = load ptr, ptr (%[-\w$.]+), align \d+.*!invariant\.load')
RE_VT_FIELD = re.compile(r'ptr (@(?:"[^"]+"|[-\w$.]+)|null)|\[(\d+) x i8\]')

ALLOC_ENTRY = {"__rustc::__rust_alloc", "__rustc::__rust_alloc_zeroed", "__rustc::__rust_realloc", "__rust_alloc",
               "__rust_alloc_zeroed", "__rust_realloc"}
# a home: the bytes get an owner by construction (round 1 report section 1, H1-H4; H5 = the accounting prototype's operator
# arena on malisper/wip/v5l-memlead-ledger2r, gravity_fn::arena OpArena / AVec / AMap / ArenaAlloc). Calls into these are
# TRACKED, never followed. allocator_api2 is NOT a home by name (round 1 had it): its collections allocate through their A,
# so the walk itself decides: A = mcx::Mcx stops at mcx:: (or its inline chain through mcx's files), A = the program arena's
# ArenaRef is a Gravity crate fn (a home by homes.tsv), A = allocator_api2's Global reaches __rust_alloc (driver lives.rs:58).
HOME = re.compile(r"^(<)?(mcx::|gravity_program::arena::|memledger::|gravity_fn::arena::)"
                  r"|^<(mcx::|gravity_program::arena::ArenaRef|gravity_fn::arena::)")
# external (declared, body not in the module) std / alloc functions that allocate; anything else in core / std / alloc that is
# only declared is NO (listed in ext_callees.tsv for review)
# D4618 / D4619 (TD-197 LINT-FP): the collections method is anchored at the name's end (\w*$: push_back, reserve_exact):
# unanchored, the MODULE btree::map::entry matched and every function in it (OccupiedEntry::remove_kv under BTreeMap::remove,
# OccupiedEntry::get) read ALLOC
EXT_ALLOC = re.compile(
    r"^alloc::(alloc::(exchange_malloc|alloc|alloc_zeroed|realloc)$|raw_vec::.*(grow|reserve|allocate_in|try_allocate|with_capacity|finish_grow|shrink)|fmt::format|vec::from_elem|slice::.*(to_vec|into_vec|concat|join|repeat|stable_sort|sort)"
    r"|str::.*(to_lowercase|to_uppercase|replace|repeat|to_owned|into_string)|string::String::(push|insert|reserve|with_capacity|from_utf8_lossy|into_boxed_str|extend_from_within|try_reserve)"
    r"|vec::Vec<T(,A)?>::(push|insert|extend|append|resize|reserve|try_reserve|with_capacity|split_off|into_boxed_slice|shrink|splice|clone_from)"
    r"|collections::.*::(insert|push|entry|append|reserve|with_capacity|split_off|resize|extend)\w*$|rc::Rc<.*>::(new|make_mut|pin|from)|sync::Arc<.*>::(new|make_mut|pin|from)"
    r"|boxed::Box<.*>::(new|pin|from)|ffi::c_str::CString::new|borrow::Cow<.*>::(into_owned|to_mut))"
    r"|^<alloc::(vec::Vec|string::String|boxed::Box|collections::|ffi::c_str::CString).* as core::clone::Clone>::clone"
    r"|^<.* as alloc::(string::ToString|borrow::ToOwned|vec::spec_\w+::\w+|vec::spec_from_iter\w*::\w+)(<.*>)?>::"
    r"|^<alloc::.* as core::iter::traits::collect::(FromIterator|Extend)<.*>>::"
    r"|^<alloc::(string::String|vec::Vec<.*>) as core::fmt::Write>::"
    r"|^<.* as core::convert::From<.*>>::from$"
    r"|^std::(thread::|collections::hash::(map|set)::.*::(insert|entry|reserve|with_capacity|extend))|^hashbrown::raw::.*(reserve|resize|with_capacity|insert|new_uninitialized)")
# a by-name allocating API (EXT_ALLOC) instantiated with a HOME's allocator among its generic arguments allocates through the
# home (TD-197 LINT-FP D4614, alloclint 2026-10-15: tpde view.rs's PgFxHashMap<'_, K, V> = hashbrown over mcx::Mcx read as 14
# global sites): the instance's debug name spells its arguments (insert<u32, gravity_tpde::view::VInst, .., mcx::Mcx>; legacy
# mangling drops them). Only a TOP-LEVEL argument counts (a key type Vec<u8, mcx::Mcx> in a Global map is not the map's
# allocator), and an instance naming a Global allocator at top level as well is not proven.
HOME_ALLOC_ARG = re.compile(r"^&?(mcx::Mcx|gravity_program::arena::ArenaRef|gravity_fn::arena::(OpArena|ArenaAlloc))$")
GLOBAL_ALLOC_ARG = re.compile(r"^&?(alloc::alloc::Global|allocator_api2::stable::alloc::global::Global|std::alloc::System)$")


def top_args(title):
    """the top-level generic arguments of a debug name `f<A, B<C, D>, E>` ([] for none)"""
    i = title.find("<")
    if i < 0 or not title.endswith(">"):
        return []
    out, depth, cur = [], 0, []
    for ch in title[i + 1:-1]:
        if ch in "<([":
            depth += 1
        elif ch in ">)]":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    out.append("".join(cur).strip())
    return out


def home_instance(title):
    """the instance's generic arguments name a home allocator at top level and no global one"""
    args = top_args(title)
    return any(HOME_ALLOC_ARG.match(x) for x in args) and not any(GLOBAL_ALLOC_ARG.match(x) for x in args)
EXT_FROM_ALLOC = re.compile(r"^<alloc::(string::String|vec::Vec|boxed::Box|rc::Rc|sync::Arc|ffi::c_str::CString|collections::)")
# the source text around a function-address reference that hands the function to the program or the driver (generated code's
# hosts, the driver's protos and owner constructors, hand pipelines, the spill hook, exec's object ops in abi::Head and its
# OwnerFns tables, hosts' AggHead): such a function is a RUNTIME root
HOST_MARK = re.compile(r"as \*const \(\)|host_words!|declare_host|\bhost\(|host_final\(|declare_proto|Proto::new|ctor_desc|Code::hand"
                       r"|set_spill_hook|hosts\[|\bcall\(b,|OwnerDesc|OwnerFns\s*\{|\bHead\s*\{|AggHead\s*\{|set_op\(|result_op")
# impls of these traits are called by the driver / the pool at run time (scope list, report section 1)
RUNTIME_TRAITS = re.compile(r" as gravity_driver::[\w:]*::(Merge|Sink|SinkLeg|BatchFold|Source|Writer|Locker|SpillFile|RunReader"
                            r"|SpillSet|SpillFactory|Attached|OnDemand)(<[^>]*>)?>::"
                            r"| as gravity_parallel::[\w:]*::(Pool|MorselSource)(<[^>]*>)?>::| as gravity_kernels::contract::Host>::")
# the host! macro's file: a function whose debug subprogram sits here and whose body is another file's is a host! fn
HOST_MACRO_FILE = "crates/gravity/abi/src/host.rs"
PRIVATE_G = re.compile(r"^(vtable\.|anon\.|__unnamed|alloc_[0-9a-f]+$|\d+$)")
STD_CRATES = {"core", "alloc", "std", "hashbrown", "compiler_builtins", "libc", "std_detect", "panic_unwind", "rustc_demangle"}
# std's thread-local access: its only allocation is the first touch's destructor registration on a thread (bounded, std's)
TLS = re.compile(r"^std::thread::local::LocalKey<.*>::|^std::thread::current::current$")
# D4694 (LINT-FP; memacct GO 2026-10-16 under the user's 10-15 ruling: a lint false positive is fixed in the lint, not converted):
# EXT_ALLOC's by-name '^std::thread::' reads every std::thread fn as allocating (spawn, Builder and scope do). These exact debug
# names allocate nothing in std's source (toolchain 1.96.0, library/std/src; a panic's own payload aside, as for any core assert),
# so a renamed or moved one stays flagged:
#   functions::yield_now  thread/functions.rs:167 -> sys/thread/unix.rs:767 (libc::sched_yield)
#   functions::sleep      thread/functions.rs:292 -> sys/thread/unix.rs:523 (a clock_nanosleep loop on a stack timespec)
#   functions::sleep_ms   thread/functions.rs:250 -> sleep
#   functions::panicking  thread/functions.rs:220 -> panicking.rs:463 count_is_zero (an atomic; the const-init TLS cell of :383)
#   Thread::id            thread/thread.rs:203 (a field read)
#   Thread::name          thread/thread.rs:246 (a field read; main_thread::get is an atomic, thread/main_thread.rs:22 / :41)
#   Thread::unpark        thread/thread.rs:183 -> the parker's wake (sys/sync/thread_parking/futex.rs:89)
# NOT here: park / park_timeout (thread/functions.rs:529 / :596) call current(), which can make the Thread handle lazily on a
# thread std did not spawn (our pthread_create threads), and neither is anything else that goes through current().
THREAD_NOALLOC = re.compile(r"^std::thread::(functions::(yield_now|sleep|sleep_ms|panicking)|thread::Thread::(id|name|unpark))$")


def crate_of(n):
    """the crate whose code a demangled function name runs: <X as Y>::m -> X's crate when X is a path, else Y's."""
    if n.startswith("<"):
        depth, i = 0, 0
        for i, ch in enumerate(n):
            if ch == "<":
                depth += 1
            elif ch == ">":
                depth -= 1
                if depth == 0:
                    break
        inner = n[1:i]
        x, _, y = inner.partition(" as ")
        x = x.lstrip("&*").replace("mut ", "").replace("const ", "")
        if "::" in x.split("<", 1)[0]:
            return x.split("::", 1)[0]
        return (y or x).split("::", 1)[0]
    return n.split("::", 1)[0]


def fields(s):
    return {k: v for k, v in RE_FIELD.findall(s)}


class Module:
    def __init__(self):
        self.dname = {}          # sym -> demangled name (the IR's own comment)
        self.defined = set()
        self.fn_sp = {}          # sym -> DISubprogram id
        self.calls = collections.defaultdict(list)   # sym -> [(callee sym | None, dbg, vtable offset | None)]
        self.refs = collections.defaultdict(list)    # sym -> [(referenced sym, dbg)]
        self.vtables = {}        # global -> [(offset, sym)]
        self.vt_refs = collections.defaultdict(list)  # sym -> [(vtable global, dbg)]
        self.md = {}             # id -> (kind, fields)
        self.fdbg = collections.defaultdict(set)     # sym -> every !dbg id on its instructions
        self.gref = {}           # global -> every @symbol its initializer names (static tables of trait objects, fn pointers)
        self.g_refs = collections.defaultdict(set)   # sym -> globals its instructions name

    def parse(self, path):
        cur, comment, pend = None, None, None
        gep, vload = {}, {}
        with open(path, errors="replace") as f:
            for line in f:
                c0 = line[:1]
                if c0 == ";":
                    if not line.startswith("; Function Attrs") and not line.startswith("; call ") and not line.startswith("; invoke "):
                        comment = line[2:].rstrip("\n")
                    continue
                if c0 == "!":
                    m = RE_MD.match(line)
                    if m:
                        self.md[int(m.group(1))] = (m.group(2), fields(m.group(3)))
                    continue
                if c0 == "d" and (line.startswith("define") or line.startswith("declare")):
                    m = RE_DEF.match(line)
                    if m:
                        sym = m.group(2).strip('"')
                        if comment and not comment.startswith("Function Attrs"):
                            self.dname[sym] = comment
                        if m.group(1) == "define":
                            self.defined.add(sym)
                            d = RE_DBG.search(line)
                            if d:
                                self.fn_sp[sym] = int(d.group(1))
                            cur, gep, vload = sym, {}, {}
                    comment = None
                    continue
                if c0 == "@":
                    if " = " in line and "ptr @" in line:
                        gname = line.split(" ", 1)[0][1:].strip('"')
                        body = line.split(" = ", 1)[1]
                        self.gref[gname] = [rm.group(1).strip('"') for rm in RE_SYM.finditer(body)]
                    if " constant " in line and "ptr @" in line and line.startswith("@vtable"):
                        name = line.split(" ", 1)[0][1:]
                        init = line.split("<{", 2)
                        body = init[2] if len(init) > 2 else line
                        off, ents = 0, []
                        for fm in RE_VT_FIELD.finditer(body):
                            if fm.group(2):
                                off += int(fm.group(2))
                            else:
                                if fm.group(1) != "null":
                                    ents.append((off, fm.group(1)[1:].strip('"')))
                                off += 8
                        self.vtables[name] = ents
                    continue
                if c0 == "}":
                    cur = None
                    continue
                if cur is None:
                    continue
                if pend is not None:      # the invoke's `to label` line carries its !dbg
                    d = RE_DBG.search(line)
                    callee, voff, refs = pend
                    pend = None
                    self._add(cur, callee, voff, refs, int(d.group(1)) if d else None, is_call=True)
                    continue
                dm = RE_DBG.search(line)
                if dm:
                    self.fdbg[cur].add(int(dm.group(1)))
                if "getelementptr inbounds" in line:
                    m = RE_GEP.match(line)
                    if m:
                        gep[m.group(1)] = (m.group(2), int(m.group(3)))
                if "!invariant.load" in line:
                    m = RE_LOAD.match(line)
                    if m:
                        src = m.group(2)
                        vload[m.group(1)] = gep[src][1] if src in gep else 0
                m = RE_CALL.search(line) if ("call " in line or "invoke " in line) else None
                callee, voff, cpos = None, None, -1
                if m and "asm " not in line[:m.start(2)]:
                    tok = m.group(2)
                    if tok[0] == "@":
                        callee = tok[1:].strip('"')
                    else:
                        voff = vload.get(tok, -1)
                    cpos = m.start(2)
                refs = [(rm.group(1).strip('"'), rm.start()) for rm in RE_SYM.finditer(line) if rm.start() != cpos]
                if m is None and not refs:
                    continue
                d = RE_DBG.search(line)
                if m and m.group(1) == "invoke" and not d:
                    pend = (callee, voff, refs)
                    continue
                self._add(cur, callee, voff, refs, int(d.group(1)) if d else None, is_call=bool(m))

    def _add(self, cur, callee, voff, refs, dbg, is_call=True):
        if is_call and (callee is not None or voff is not None):
            if callee is None or not callee.startswith("llvm."):
                self.calls[cur].append((callee, dbg, voff))
        for s, _ in refs:
            if s.startswith("vtable."):
                self.vt_refs[cur].append((s, dbg))
            elif s.startswith("alloc_") or s.startswith("anon.") or s in self.gref:
                self.g_refs[cur].add(s)
            elif not s.startswith("llvm."):
                self.refs[cur].append((s, dbg))

    # ---- debug info ----
    def file_of_scope(self, sid, depth=0):
        k = self.md.get(sid)
        if not k or depth > 64:
            return None
        kind, fl = k
        if "file" in fl and fl["file"].startswith("!"):
            fk = self.md.get(int(fl["file"][1:]))
            if fk and fk[0] == "DIFile":
                return fk[1].get("filename", "").strip('"')
        if "scope" in fl and fl["scope"].startswith("!"):
            return self.file_of_scope(int(fl["scope"][1:]), depth + 1)
        return None

    def sp_of_scope(self, sid, depth=0):
        k = self.md.get(sid)
        if not k or depth > 64:
            return None
        if k[0] == "DISubprogram":
            return sid
        sc = k[1].get("scope", "")
        return self.sp_of_scope(int(sc[1:]), depth + 1) if sc.startswith("!") else None

    def frames(self, lid):
        """innermost-first [(file, line, sp id)] of a DILocation and its inlinedAt chain."""
        out, seen = [], 0
        while lid is not None and seen < 64:
            k = self.md.get(lid)
            if not k or k[0] != "DILocation":
                break
            fl = k[1]
            sc = int(fl["scope"][1:]) if fl.get("scope", "").startswith("!") else None
            out.append((self.file_of_scope(sc) if sc else None, int(fl.get("line", "0")), self.sp_of_scope(sc) if sc else None))
            ia = fl.get("inlinedAt", "")
            lid = int(ia[1:]) if ia.startswith("!") else None
            seen += 1
        return out




# a call whose inline chain passes through a home's own code is that home's allocation (O0 still inlines #[inline(always)]:
# allocator_api2's PgVec::push -> finish_grow<Mcx> -> Mcx::allocate -> the mcx heap block -> __rust_alloc is one call site
# in the caller's body). Gravity's own homes are checked by homes.tsv function spans (Homes.of) on the frame's line.
HOME_FILE = re.compile(r"(^|/)crates/_support/(mcx|memledger)/|(^|/)crates/gravity/program/src/arena\.rs|(^|/)crates/gravity/fn/src/arena")


def load_scope(path):
    out = []
    for ln in open(path):
        if ln.startswith("#") or not ln.strip():
            continue
        c = ln.rstrip("\n").split("\t")
        out.append((c[0].rstrip("/"), c[1], c[2] if len(c) > 2 else ""))
    return out


def load_roots(path):
    rows = []
    if path and os.path.exists(path):
        for ln in open(path):
            if ln.startswith("#") or not ln.strip():
                continue
            c = ln.rstrip("\n").split("\t")
            if len(c) >= 2:
                rows.append((c[0], re.compile(c[1]), c[1], c[2] if len(c) > 2 else ""))
    return rows


class Union:
    """the whole-program view: every module absorbed, metadata resolved per module and dropped"""

    def __init__(self, crates, homes):
        self.dirs = sorted(crates, key=len, reverse=True)   # longest first: backend/interp before backend
        self.pkg_dir = {p: d for d, p in crates.items()}
        self.homes = homes
        self.dname, self.defined = {}, set()
        self.calls = collections.defaultdict(list)   # sym -> [(callee | None, voff | None, frame | None, home flag)]
        self.refs = collections.defaultdict(list)    # sym -> [(referenced sym, frame | None)]
        self.vtables, self.gref = {}, {}
        self.vt_refs = collections.defaultdict(set)
        self.g_refs = collections.defaultdict(set)
        self.crate_fn = {}                           # sym -> crate dir
        self.fn_file, self.fn_line, self.fn_lo, self.fn_hi = {}, {}, {}, {}
        self.sp_title = {}                           # sym -> its DISubprogram's name (generic arguments spelled: new<..PgError>)
        self.inl_title = collections.defaultdict(set)  # (file, line, callee sym) -> the innermost inlined frame's subprogram name
        #                                              (Box::new<PgError> inlined around its box_new_uninit / exchange_malloc call)
        self.macro_fns, self.host_fns = set(), set()
        self.modules = 0
        self.fcache = {}

    def file_crate(self, f):
        if not f:
            return None, None
        if f in self.fcache:
            return self.fcache[f]
        r = (None, None)
        for c in self.dirs:
            i = f.find(c + "/src/")
            if i >= 0:
                if c in findings_mod.SCOPE_FILES and f[i + len(c) + 1:] not in findings_mod.SCOPE_FILES[c]:
                    break   # another file of a crate scoped by file (scope.tsv's 4th column): not a linted crate's code
                r = (f[i:], c)
                break
        self.fcache[f] = r
        return r

    def absorb(self, mod, tag):
        ns = lambda g: f"{tag}:{g}" if PRIVATE_G.match(g) else g  # noqa: E731
        self.modules += 1
        dn = mod.dname
        for s, n in dn.items():
            self.dname.setdefault(s, n)
        self.defined |= mod.defined
        fr_memo, site_memo, home_memo = {}, {}, {}

        def frames(d):
            if d not in fr_memo:
                fr_memo[d] = mod.frames(d)
            return fr_memo[d]
        sp_name = {sp: dn.get(s, s) for s, sp in mod.fn_sp.items()}
        for s_, sp_ in mod.fn_sp.items():
            k_ = mod.md.get(sp_)
            if k_ and k_[0] == "DISubprogram":
                self.sp_title.setdefault(s_, k_[1].get("name", "").strip('"'))

        def frame_title(sp):
            k = mod.md.get(sp) if sp is not None else None
            return k[1].get("name", "").strip('"') if k else ""

        def frame_fn(sp):
            if sp in sp_name:
                return sp_name[sp]
            k = mod.md.get(sp)
            return k[1].get("name", "?").strip('"') if k else "?"

        def site_frame(d):
            """(file, line, frame fn) of the innermost frame in a linted crate"""
            if d is None:
                return None
            if d not in site_memo:
                r = None
                for f, ln, sp in frames(d):
                    nf, _ = self.file_crate(f)
                    if nf:
                        r = (nf, ln, frame_fn(sp))
                        break
                site_memo[d] = r
            return site_memo[d]

        def inlined_home(d, stop_at_crate):
            """the call's inline chain runs through a home's code (before it reaches a linted frame, if stop_at_crate)"""
            if d is None:
                return False
            key = (d, stop_at_crate)
            if key not in home_memo:
                r = False
                for i, (f, ln, _) in enumerate(frames(d)):
                    nf, _ = self.file_crate(f)
                    if nf and self.homes.of(nf, ln):
                        r = True
                        break
                    if stop_at_crate and nf:
                        break
                    if f and HOME_FILE.search(f):
                        r = True
                        break
                home_memo[key] = r
            return home_memo[key]

        # crate functions of this module: their debug subprogram's file is a linted crate's; a host! fn (subprogram in the
        # macro's file) and a fn of a linted crate whose subprogram sits in another crate's macro count by their body's lines
        local_crate = {}
        for s, sp in mod.fn_sp.items():
            k = mod.md.get(sp)
            if not k:
                continue
            nf, c = self.file_crate(mod.file_of_scope(sp))
            name = dn.get(s, s)
            ncrate = crate_of(name)
            in_macro = nf == HOST_MACRO_FILE
            if nf and not in_macro:
                local_crate[s] = (c, nf, int(k[1].get("line", "0")))
                continue
            want = self.pkg_dir.get(ncrate)
            if want is None and not in_macro:
                continue
            files, first = collections.Counter(), {}
            for did in mod.fdbg.get(s, ()):
                for ff, ln, _ in frames(did):
                    f2, c2 = self.file_crate(ff)
                    if f2 and ln > 0 and f2 != HOST_MACRO_FILE and (want is None or c2 == want):
                        files[(f2, c2)] += 1
                        first[f2] = min(first.get(f2, ln), ln)
            if files:
                (f2, c2), _ = files.most_common(1)[0]
                local_crate[s] = (c2, f2, first[f2])
                self.macro_fns.add(s)
                if in_macro:
                    self.host_fns.add(s)
            elif in_macro:
                local_crate[s] = ("crates/gravity/abi", nf, int(k[1].get("line", "0")))
        for s, (c, f, line) in local_crate.items():
            self.crate_fn[s] = c
            self.fn_file.setdefault(s, f)
            self.fn_line.setdefault(s, line)
            lo, hi = self.fn_lo.get(s, line), self.fn_hi.get(s, line)
            for did in mod.fdbg.get(s, ()):
                fr = frames(did)
                if fr:
                    nf, _ = self.file_crate(fr[-1][0])
                    ln = fr[-1][1]
                    if nf == f and ln > 0:
                        lo, hi = min(lo, ln), max(hi, ln)
            self.fn_lo[s], self.fn_hi[s] = lo, hi
        # calls, references, vtables, statics
        for s, cl in mod.calls.items():
            is_crate = s in local_crate
            out = self.calls[s]
            for callee, dbg, voff in cl:
                sf = site_frame(dbg) if (is_crate or voff is not None) else None
                out.append((callee, voff, sf, inlined_home(dbg, is_crate)))
                if sf and callee is not None and dbg is not None:
                    fl = frames(dbg)
                    if fl and not self.file_crate(fl[0][0])[0]:   # the call sits in inlined std code: name that frame
                        self.inl_title[(sf[0], sf[1], callee)].add(frame_title(fl[0][2]))
        for s, rl in mod.refs.items():
            is_crate = s in local_crate
            for r, dbg in rl:
                if r in mod.defined or r in dn:
                    self.refs[s].append((r, site_frame(dbg) if is_crate else None))
        for v, ents in mod.vtables.items():
            self.vtables[ns(v)] = ents
        for g, refs in mod.gref.items():
            self.gref[ns(g)] = [ns(x) for x in refs]
        for s, vs in mod.vt_refs.items():
            self.vt_refs[s].update(ns(v) for v, _ in vs)
        for s, gs in mod.g_refs.items():
            self.g_refs[s].update(ns(g) for g in gs)


def family(f, crate):
    """the operator family a file belongs to: <crate>/src/**/ops/<op>/.. -> op; kernels' lab par_* and the agg tables by
    name; the run machinery (driver, parallel, pool, abi, wire, interp) and expression evaluation (fn, expr, dict) by crate"""
    if not crate:
        return "-"
    rest = f[len(crate) + 5:]
    parts = rest.split("/")
    short = crate.rsplit("/", 1)[-1]
    if short == "lowerv4":     # round 1's names
        return parts[1] if parts[0] == "ops" and len(parts) > 2 else "shared:" + parts[0].replace(".rs", "")
    if "ops" in parts[:-1]:
        i = parts.index("ops")
        if i + 1 < len(parts):
            op = parts[i + 1].replace(".rs", "")
            if op not in ("mod", "shared"):
                return op
    first = parts[0].replace(".rs", "")
    fam = {("kernels", "par_hashagg"): "agg", ("kernels", "par_hashagg_codes"): "agg", ("kernels", "table"): "agg",
           ("exec", "table"): "agg", ("kernels", "par_hashjoin"): "hashjoin", ("kernels", "par_sort"): "sort",
           ("kernels", "par_topk"): "sort", ("kernels", "par_distinct"): "group", ("kernels", "par_scan_count"): "scan",
           ("kernels", "dict"): "expr:dict", ("hosts", "dict"): "expr:dict"}.get((short, first))
    if fam:
        return fam
    if short in ("driver", "parallel", "pool", "abi", "wire", "interp"):
        return "run:" + short
    if short in ("fn", "expr", "dict"):
        return "expr:" + short
    return short + ":" + first


def crate_deps(root, crates):
    """package -> the linted packages it may name (its [dependencies] / [target.*.dependencies], transitively; dev-dependencies
    are not linked into the lib): a dyn call in crate X's code can only reach a trait X or its dependencies define"""
    pkgs = set(crates.values())
    direct = {}
    for d, p in crates.items():
        deps, sec = set(), ""
        try:
            text = open(os.path.join(root, d, "Cargo.toml")).read()
        except OSError:
            text = ""
        for ln in text.splitlines():
            t = ln.strip()
            if t.startswith("["):
                sec = t
                continue
            m = re.match(r"([\w-]+)\s*=", t)
            if m and sec.endswith("dependencies]") and "dev-dependencies" not in sec and m.group(1).replace("-", "_") in pkgs:
                deps.add(m.group(1).replace("-", "_"))
        direct[p] = deps
    out = {}
    for p in pkgs:
        seen, st = {p}, [p]
        while st:
            for q in direct.get(st.pop(), ()):
                if q not in seen:
                    seen.add(q)
                    st.append(q)
        out[p] = seen
    return out


def trait_of(n):
    """a vtable entry's trait: `<T as krate::path::Trait<..>>::m` -> (krate, Trait); a provided method
    `krate::path::Trait::m` -> (krate, Trait); None when it cannot tell"""
    if not n.startswith("<"):
        segs = n.split("::")
        return (segs[0], segs[-2]) if len(segs) >= 3 else None
    depth = 0
    for i, ch in enumerate(n):
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth -= 1
            if depth == 0:
                inner = n[1:i]
                k = inner.rfind(" as ")
                if k < 0:
                    return None
                path = inner[k + 4:].split("<", 1)[0]
                segs = path.split("::")
                return (segs[0], segs[-1])
    return None


def ll_files(paths):
    out = []
    for p in paths:
        if os.path.isdir(p):
            for d, _, fs in sorted(os.walk(p)):
                out += [os.path.join(d, f) for f in sorted(fs) if f.endswith(".ll")]
        else:
            out.append(p)
    return out



def statement_window(src_line, f, ln):
    """the lines of the statement a finding at `ln` starts: `ln` and the lines after it up to the first that ends with ; { } ,
    (at most 12). `src_line(f, n)` is line n of file f (1-based, '' past the end)."""
    out = [ln]
    for i in range(ln, ln + 12):
        t = src_line(f, i).split("//")[0].rstrip()
        if i > ln:
            out.append(i)
        if t.endswith((";", "{", "}", ",")) and i >= ln:
            break
    return out


def proof_window(src_line, f, ln):
    """the lines a finding's IR verdict (ir_of: ALLOC / NOALLOC) reads: its statement and the three lines above it"""
    return statement_window(src_line, f, ln) + [x for x in range(ln - 3, ln) if x > 0]


def continued_from(src_line, f, ln):
    """the lines above `ln` (at most three) whose statement runs on to `ln`: each line up from ln-1 until one that ends a
    statement (; { } ,)"""
    out = []
    for x in range(ln - 1, max(ln - 4, 0), -1):
        if src_line(f, x).split("//")[0].rstrip().endswith((";", "{", "}", ",")):
            break
        out.append(x)
    return out


def homed_window(src_line, f, ln):
    """the lines that decide HOMED for a NOALLOC finding: its own statement, and the lines above that open it (a finding on
    a continuation line)"""
    return statement_window(src_line, f, ln) + continued_from(src_line, f, ln)


def noalloc_class(homed_at, src_line, f, ln):
    """D4672: a NOALLOC finding's named class: HOMED when a call in its own statement allocates only through a home
    (homed_at: (file, line) of such a call), else NOALLOC. Never a fixed l-3 .. l+11: that one reached the next
    statements (fetch_site.rs:182's one-line Arc clone read HOMED once a `charged` tag made the push 11 lines below it a
    home, the S3 alloclint-ir RED of 0ad329c7a01), and not the three lines above unless the statement starts there
    (index_leaves/mod.rs:457's `self.filed.clone()` read HOMED from `if self.cands.is_empty() {` 3 lines above once
    cands became a ChargedVec). Both classes are named, not debt: the NOALLOC proof itself (ir_of, proof_window) is
    unchanged."""
    return "HOMED" if any((f, x) in homed_at for x in homed_window(src_line, f, ln)) else "NOALLOC"


def by_name_alloc(n):
    """a std / alloc / hashbrown allocating API by its debug name `n` (EXT_ALLOC), the THREAD_NOALLOC names out (D4694)"""
    root = n.lstrip("<").split("::", 1)[0]
    return (root in ("alloc", "std", "hashbrown") or n.startswith("<")) and bool(EXT_ALLOC.search(n)) \
        and (not n.endswith(">::from") or bool(EXT_FROM_ALLOC.search(n))) and not THREAD_NOALLOC.match(n)


def name_class(n, title="", defined=False, entry=False):
    """the allocation class of a non-crate callee by its debug name `n` (`title`: its subprogram title; `defined`: the union
    holds its body; `entry`: its symbol is an ALLOC_ENTRY): ENTRY / HOME / NO / ALLOC / DEP, or None (walk its body)"""
    if entry or n in ALLOC_ENTRY:
        return "ENTRY"
    if HOME.search(n):
        return "HOME"
    if TLS.search(n) or THREAD_NOALLOC.match(n):
        return "NO"
    root = n.lstrip("<").split("::", 1)[0]
    if by_name_alloc(n):
        if home_instance(title):
            return "HOME"   # the instance's allocator is a home (HOME_ALLOC_ARG, D4614)
        # a std allocating API by NAME, defined in the union or not: where a shared generic's instance is defined is a build
        # artifact (round 3 MEASURED: with pgrc_meta in the union, exec's `Vec<usize>::push` call resolves to pgrc_meta's
        # exported instance, whose walk through push_mut said NO, and 56 lines lost their ALLOC)
        return "ALLOC"
    if defined:
        return None
    if "dealloc" in n or n.startswith("core::ptr::drop_in_place") or n.endswith(" as core::ops::drop::Drop>::drop"):
        return "NO"
    if root in ("alloc", "std", "hashbrown") or n.startswith("<"):
        return "NO"   # (by_name_alloc's ALLOC returned above)
    if root in ("core", "libc", "compiler_builtins") or root.startswith("llvm") or root.startswith("__"):
        return "NO"
    return "DEP"


def selftest_thread_noalloc():
    """D4694: the named non-allocating std::thread fns read NO, defined in the union or not; every other std::thread fn keeps
    its by-name ALLOC (a planted spawn / scope / park still flags)"""
    clean = ["std::thread::functions::yield_now", "std::thread::functions::sleep", "std::thread::functions::sleep_ms",
             "std::thread::functions::panicking", "std::thread::thread::Thread::id", "std::thread::thread::Thread::name",
             "std::thread::thread::Thread::unpark"]
    for n in clean:
        assert EXT_ALLOC.search(n), n   # the rule this list narrows names it
        assert not by_name_alloc(n), n
        assert name_class(n) == "NO" and name_class(n, defined=True) == "NO", n
    flagged = ["std::thread::functions::spawn", "std::thread::builder::Builder::spawn", "std::thread::Builder::spawn",
               "std::thread::scoped::scope", "std::thread::scoped::Scope::spawn", "std::thread::functions::park",
               "std::thread::functions::park_timeout", "std::thread::functions::available_parallelism",
               "std::thread::functions::sleep_until", "std::thread::thread::Thread::new",
               "std::thread::functions::yield_now_then", "std::thread::thread::Thread::identity"]
    for n in flagged:
        assert by_name_alloc(n), n
        assert name_class(n) == "ALLOC" and name_class(n, defined=True) == "ALLOC", n
    # outside std::thread nothing moves: a by-name ALLOC stays ALLOC, a non-allocating std fn stays NO, TLS stays NO
    assert name_class("alloc::raw_vec::RawVec<T,A>::grow_one") == "ALLOC"
    assert name_class("std::io::stdio::_print") == "NO" and name_class("std::io::stdio::_print", defined=True) is None
    assert name_class("std::thread::current::current") == "NO"
    assert name_class("other_crate::thread::functions::sleep") == "DEP"
    print("crosscheck selftest (D4694 thread names): ok")


def selftest():
    """the D4672 window rules on a fixture shaped like fetch_site.rs's group()"""
    src = {"a.rs": [
        "fn group(&mut self) -> Result<()> {",                                 # 1
        "    let id = rowids[self.order[from] as usize];",                    # 2
        "    let (p, lg) = (rowid_part(id) as usize, rowid_granule(id));",    # 3
        "    self.pin(p)?;",                                                  # 4
        "    let list = Arc::clone(self.list.as_ref().expect(\"bound\"));",   # 5  the finding
        "    let deletes = list.parts()[p].deletes.as_ref();",                # 6
        "    self.sel.clear();",                                              # 7
        "    for &i in &self.order[from..to] {",                              # 8
        "        let row = rowid_row(rowids[i as usize]);",                   # 9
        "        if self.sel.last() != Some(&(row as u16)) {",                # 10
        "            self.sel.push(row as u16); // alloc: Node charged o",    # 11
        "        }",                                                          # 12
        "        self.didx.push(self.sel.len() as u16 - 1); // alloc: Node charged o",  # 13
        "    }",                                                              # 14
        "    let v: Vec<u8, ArenaRef> = self",                                # 15 a multi-line statement
        "        .bytes",                                                     # 16
        "        .clone();",                                                  # 17
        "    Ok(())",                                                         # 18
        "}",                                                                  # 19
    ]}
    sl = lambda f, n: src[f][n - 1] if 0 < n <= len(src[f]) else ""  # noqa: E731
    assert statement_window(sl, "a.rs", 5) == [5], statement_window(sl, "a.rs", 5)
    assert proof_window(sl, "a.rs", 5) == [5, 2, 3, 4], proof_window(sl, "a.rs", 5)
    # the tagged push 6 lines below (and the one 8 below) is another statement: the clone stays NOALLOC
    assert noalloc_class({("a.rs", 11), ("a.rs", 13)}, sl, "a.rs", 5) == "NOALLOC"
    # the old fixed window l-3 .. l+11 did reach it (the RED this replaces)
    assert any(("a.rs", x) in {("a.rs", 11)} for x in range(5 - 3, 5 + 12))
    # a home call on the finding's own line is HOMED; one in a finished statement above it is not (index_leaves:457)
    assert noalloc_class({("a.rs", 5)}, sl, "a.rs", 5) == "HOMED"
    assert noalloc_class({("a.rs", 3)}, sl, "a.rs", 5) == "NOALLOC"
    assert continued_from(sl, "a.rs", 5) == []
    # a finding on a continuation line: the lines that open its statement count
    assert continued_from(sl, "a.rs", 17) == [16, 15], continued_from(sl, "a.rs", 17)
    assert noalloc_class({("a.rs", 15)}, sl, "a.rs", 17) == "HOMED"
    assert noalloc_class({("a.rs", 14)}, sl, "a.rs", 17) == "NOALLOC"
    # a statement over several lines: its continuation lines are its own, the line after its ';' is not
    assert statement_window(sl, "a.rs", 15) == [15, 16, 17], statement_window(sl, "a.rs", 15)
    assert noalloc_class({("a.rs", 17)}, sl, "a.rs", 15) == "HOMED"
    assert noalloc_class({("a.rs", 18)}, sl, "a.rs", 15) == "NOALLOC"
    # another file's home line never counts
    assert noalloc_class({("b.rs", 5)}, sl, "a.rs", 5) == "NOALLOC"
    selftest_thread_noalloc()
    print("crosscheck selftest: ok")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--ll", nargs="+", required=True, help=".ll files or directories of them (collect.sh's OUT/ir)")
    ap.add_argument("--clippy", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--crate", action="append", default=[], help="a crate dir of scope.tsv (repeatable; default: every one)")
    ap.add_argument("--scope", default=os.path.join(HERE, "scope.tsv"))
    ap.add_argument("--homes", default=os.path.join(HERE, "homes.tsv"))
    ap.add_argument("--roots", default=os.path.join(HERE, "roots.tsv"))
    ap.add_argument("--paths", default=os.path.join(HERE, "paths.tsv"))
    ap.add_argument("--sizes", default=os.path.join(HERE, "size_class.tsv"),
                    help="size_class.tsv: file<TAB>line<TAB>class<TAB>why (the reviewed runtime rows)")
    ap.add_argument("--why", action="append", default=[], help="print a root path to each function whose name has this text")
    ap.add_argument("--callers", action="append", default=[], help="print the caller tree (6 levels up) of a function whose name has this text")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    scope = load_scope(a.scope)
    # (W3 load witness, 2026-10-14: par.rs:576 669 MiB was auto-BOUNDED as UNREACHED) a server crate of the scope (role
    # RUNTIME, outside crates/gravity) is entered from outside the union (COPY -> pgrc_ingest_par::parallel_ingest, the
    # table AM callbacks, the spill set's writers): a function there the walk from Gravity's roots does not reach is RUNTIME
    server_rt = {d for d, _, role in scope if role == "RUNTIME" and not d.startswith("crates/gravity/")}
    crates = {d: p for d, p, _ in scope if not a.crate or d in [c.rstrip("/") for c in a.crate]}
    homes = homes_mod.Homes(a.root, a.homes)
    U = Union(crates, homes)
    files = ll_files(a.ll)
    for i, p in enumerate(files):
        m = Module()
        m.parse(p)
        U.absorb(m, f"m{i}")
        del m
    dn = lambda s: U.dname.get(s, s)  # noqa: E731
    crate_fn = U.crate_fn
    src_cache = {}

    def src_line(f, n):
        if f not in src_cache:
            try:
                src_cache[f] = open(os.path.join(a.root, f), errors="replace").read().split("\n")
            except OSError:
                src_cache[f] = []
        lines = src_cache[f]
        return lines[n - 1] if 0 < n <= len(lines) else ""

    # ---- allocation summary of every non-crate callee ----
    memo = {}

    def ext_class(s):
        return name_class(dn(s), U.sp_title.get(s, ""), s in U.defined, s in ALLOC_ENTRY)

    rev = collections.defaultdict(set)
    home_seed = set()
    for s0 in U.defined:
        if s0 in crate_fn or ext_class(s0) is not None:
            continue
        for callee, _, _, hflag in U.calls.get(s0, ()):
            if callee is not None and callee not in crate_fn and not hflag:
                rev[callee].add(s0)
            elif hflag:
                home_seed.add(s0)   # its body inlines a home's code (an ArenaVec's grow through ArenaRef::allocate)
    # HOMED (memacct 2026-10-15T21:1xZ): the non-crate functions that reach a home (a HOME callee, or a home inlined): an
    # instance whose walk is NO (no global allocation) but that reaches a home allocates only there (an ArenaVec clone)
    home_reach = home_seed | {c for c in rev if ext_class(c) == "HOME"}
    st_h = list(home_reach)
    while st_h:
        x = st_h.pop()
        for caller in rev.get(x, ()):
            if caller not in home_reach:
                home_reach.add(caller)
                st_h.append(caller)
    for lvl in ("ALLOC", "DEP"):
        st = []
        for callee in list(rev):
            c = ext_class(callee)
            if (lvl == "ALLOC" and c in ("ENTRY", "ALLOC")) or (lvl == "DEP" and c == "DEP"):
                st.append(callee)
        st += [x for x, v in memo.items() if v == lvl]
        seen = set(st)
        while st:
            x = st.pop()
            for caller in rev.get(x, ()):
                if caller in seen or memo.get(caller) == "ALLOC":
                    continue
                memo[caller] = lvl
                seen.add(caller)
                st.append(caller)

    def allocates(s):
        c = ext_class(s)
        return c if c is not None else memo.get(s, "NO")
    linted_pkgs = set(crates.values())

    # ---- IR sites ----
    ir_sites, home_sites, dep_calls, ext_seen, dep_sites, dep_unknown = [], [], collections.Counter(), collections.Counter(), [], []
    homed_at = set()   # (file, line) of a call that allocates only through a home (an ArenaVec clone into the plan arena)
    for s in crate_fn:
        for callee, voff, fr, hflag in U.calls.get(s, ()):
            if callee is None or callee in crate_fn:
                continue
            r = allocates(callee)
            if r != "HOME" and hflag:
                r = "HOME"
            if r in ("ENTRY", "ALLOC") and crate_of(dn(callee)) not in STD_CRATES:
                r = "DEP-ALLOC"
            if r == "DEP-ALLOC":
                dep_calls[dn(callee) + "\tALLOC"] += 1
                ext_seen[(dn(callee), r)] += 1
                if fr:
                    dep_sites.append((fr[0], fr[1], fr[2], dn(s), dn(callee), s))
                # an allocating helper of a dependency crate (outside the union's linted crates and std) counts AT THE CALLER
                # (main 2026-10-14T18:15:24Z (2), cvgap: cancelpoll::Step::reserve grew text_hosts.rs:1796 / :1807's Vecs,
                # 384 MiB, and the call had no row)
                r = "ALLOC-DEP"
            if r in ("ENTRY", "ALLOC", "ALLOC-DEP"):
                if r != "ALLOC-DEP":
                    ext_seen[(dn(callee), r)] += 1
                site = (fr[0], fr[1], fr[2], dn(s), dn(callee), s, callee) if fr else \
                    (U.fn_file[s], 0, dn(s), dn(s), dn(callee), s, callee)
                h = homes.of(site[0], site[1]) if site[1] else homes.of(site[0], U.fn_line[s])
                (home_sites if h else ir_sites).append(site + ((h,) if h else ()))
            elif r == "DEP":
                dep_calls[dn(callee) + "\tunknown"] += 1
                dep_unknown.append((s, dn(callee)))
            elif r in ("NO", "HOME") and callee not in U.defined:
                ext_seen[(dn(callee), r)] += 1
            if fr and (r == "HOME" or (r == "NO" and callee in home_reach)):
                homed_at.add((fr[0], fr[1]))

    # ---- clippy findings ----
    all_findings = findings_mod.parse(a.clippy, list(crates))
    findings, home_findings = [], []
    for x in all_findings:
        h = homes.of(x[0], x[1])
        (home_findings if h else findings).append(x + ((h,) if h else ()))
    by_line = collections.defaultdict(list)
    for f, l, c, k, it in findings:
        by_line[(f, l)].append((k, it))

    # TYPE-MENTION (main 2026-10-14T23:41:27Z ruling 1): a mention covers NOTHING. A type finding on a line with no method /
    # growth / macro finding names a type and is no row (ratchet.py), so an IR allocation in its statement that no allocation
    # finding covers is a MISS: an ir-only row. An allocation the IR sees has a row somewhere.
    # REFCOUNT-CLONE (USER GO via main 2026-10-15T11:46:31Z): an Rc / Arc / Weak clone alone on its line covers nothing either
    # REFCOUNT-CLONE-INLINE (USER RULING via memacct 2026-10-15T18:5xZ): a resolved refcount clone covers nothing wherever it
    # stands (the line's other findings still cover)
    refc = findings_mod.refcount_clones(all_findings, findings_mod.refptr_spans(a.clippy))
    cover = {k for k in by_line if k in findings_mod.alloc_lines([x for x in all_findings if x not in refc])}
    refc_at = {(x[0], x[1], x[4]) for x in refc}

    def stmt_window(f, ln):
        return statement_window(src_line, f, ln)

    misses, covered = [], 0
    for site in ir_sites:
        f, l = site[0], site[1]
        hit = any((f, x) in cover for x in stmt_window(f, l)) or (f, l - 1) in cover and src_line(f, l - 1).rstrip().endswith(("=", "("))
        if hit:
            covered += 1
        else:
            misses.append(site)
    covered_lines = collections.defaultdict(set)
    fn_range = {}
    for s in crate_fn:
        fn_range[s] = (U.fn_file[s], U.fn_lo[s], U.fn_hi[s])
        covered_lines[U.fn_file[s]].update(range(U.fn_lo[s], U.fn_hi[s] + 1))
    sites_by_fn = collections.defaultdict(list)
    for site in ir_sites:
        if site[0] == U.fn_file.get(site[5]):
            sites_by_fn[site[5]].append(site)
    lines_by_file = collections.defaultdict(set)
    for (f, l) in by_line:
        lines_by_file[f].add(l)
    fmiss = []
    for s, ss in sites_by_fn.items():
        f, lo, hi = fn_range[s]
        if not any(lo <= l <= hi for l in lines_by_file.get(f, ())):
            fmiss.append((f, lo, dn(s), len(ss), ss[0][4]))
    site_lines = collections.defaultdict(set)
    callees_at = collections.defaultdict(set)
    fns_at = collections.defaultdict(set)     # (file, line) -> the IR functions holding that line's sites (inlined frames too)
    for site in ir_sites:
        site_lines[site[0]].add(site[1])
        callees_at[(site[0], site[1])].add(site[6])
        fns_at[(site[0], site[1])].add(site[5])
    for site in dep_sites:
        site_lines[site[0]].add(site[1])
        callees_at[(site[0], site[1])].add("dep:" + site[4])
    ir_of, line_callees = {}, {}
    for (f, l) in by_line:
        win = proof_window(src_line, f, l)   # its statement and the three lines above it (D4672: HOMED reads the same)
        if any(x in site_lines.get(f, ()) for x in win):
            ir_of[(f, l)] = "ALLOC"
            line_callees[(f, l)] = set().union(*(callees_at.get((f, x), set()) for x in win))
        elif l in covered_lines.get(f, ()):
            ir_of[(f, l)] = "NOALLOC"
        else:
            ir_of[(f, l)] = "BLIND"

    # ---- call graph and phase ----
    vt_by_off = collections.defaultdict(list)
    for name, ents in U.vtables.items():
        for off, sym in ents:
            vt_by_off[off].append(sym)
    vt_by_off = {k: sorted(set(v)) for k, v in vt_by_off.items()}

    def method_of(n):
        return n.rsplit("::", 1)[-1]

    def window_text(f, ln):
        return " ".join(src_line(f, i) for i in range(max(1, ln - 1), ln + 4))

    graph = collections.defaultdict(set)
    unresolved, unres_rows = 0, []
    deps_of = crate_deps(a.root, crates)
    pkg_of_dir = dict(crates)
    crate_text, named_memo = {}, {}

    def names_trait(cdir, trait):
        if (cdir, trait) not in named_memo:
            if cdir not in crate_text:
                parts = []
                base = os.path.join(a.root, cdir, "src")
                for d, _, fs in os.walk(base):
                    for f_ in fs:
                        if f_.endswith(".rs"):
                            parts.append(open(os.path.join(d, f_), errors="replace").read())
                crate_text[cdir] = "\n".join(parts)
            named_memo[(cdir, trait)] = re.search(r"\b" + re.escape(trait) + r"\b", crate_text[cdir]) is not None
        return named_memo[(cdir, trait)]
    for s in U.defined:
        for callee, voff, fr, _ in U.calls.get(s, ()):
            if callee is not None:
                graph[s].add(callee)
            elif voff is not None and voff >= 0:
                cands = vt_by_off.get(voff, ())
                if fr:
                    txt = window_text(fr[0], fr[1])
                    cands = [c for c in cands if re.search(r"(?:\.|::)\s*" + re.escape(method_of(dn(c))) + r"\s*(?:::<[^>]*>)?\s*\(", txt)]
                    # the trait must be one the calling code can name: its own crate's or a dependency's (driver code cannot
                    # call lowerv4's EndMech::open through a vtable, whatever the slot and the method name); and a trait
                    # of ANOTHER crate must be named somewhere in the calling crate's source (exec's `x.reopen(..)` is its
                    # own Protocol's, not lowerv4's SourceMech, which exec never names); if that leaves none, keep them
                    ccd = findings_mod.crate_dir(fr[0], list(crates))
                    vis = deps_of.get(pkg_of_dir.get(ccd, ""), None)
                    if vis is not None:
                        cands = [c for c in cands if (trait_of(dn(c)) or ("", ""))[0] not in linted_pkgs or trait_of(dn(c))[0] in vis]
                        named = [c for c in cands if (trait_of(dn(c)) or ("", ""))[0] not in linted_pkgs
                                 or trait_of(dn(c))[0] == pkg_of_dir.get(ccd) or names_trait(ccd, trait_of(dn(c))[1])]
                        cands = named or cands
                else:
                    cands = []
                if cands:
                    graph[s].update(cands)
                else:
                    unresolved += 1
                    unres_rows.append((s, fr[0] if fr else "?", fr[1] if fr else 0, voff))
    rt_vtables = {v for v, ents in U.vtables.items() if any(RUNTIME_TRAITS.search(dn(x)) for _, x in ents)}
    rt_vtables |= {g for g, refs in U.gref.items() if any(RUNTIME_TRAITS.search(dn(x)) for x in refs)}

    def is_extern(s):
        f, l = U.fn_file.get(s), U.fn_line.get(s, 0)
        if not f:
            return False
        return any('extern "C' in src_line(f, i) for i in range(max(1, l - 1), l + 2))
    # functions handed to generated code / the driver: called at run time wherever the hand-off is written
    handed, host_refs, addr_rows = set(), 0, set()
    for s in U.defined:
        for r, fr in U.refs.get(s, ()):
            txt = " ".join(src_line(fr[0], i) for i in range(max(1, fr[1] - 2), fr[1] + 3)) if fr else ""
            if r in U.host_fns or (r in crate_fn and is_extern(r)) or (fr and HOST_MARK.search(txt)):
                handed.add(r)
                host_refs += 1
                if fr:
                    addr_rows.add(("RUNTIME-ROOT", dn(r), f"{fr[0]}:{fr[1]}"))
    for g, refs in U.gref.items():
        for x in refs:
            if x in crate_fn and (x in U.host_fns or is_extern(x)):
                handed.add(x)
                addr_rows.add(("RUNTIME-ROOT", dn(x), "static " + g))
    gmemo = {}

    def gclose(g):
        if g in gmemo:
            return gmemo[g]
        gmemo[g] = set()
        out, st, seen = set(), [g], {g}
        while st:
            x = st.pop()
            if x in rt_vtables:
                continue
            for y in U.gref.get(x, ()):
                if y in seen:
                    continue
                seen.add(y)
                if y in U.gref or y in U.vtables:
                    st.append(y)
                elif (y in U.defined or y in U.dname) and y not in handed:
                    out.add(y)
        gmemo[g] = out
        return out
    for s in U.defined:
        for r, fr in U.refs.get(s, ()):
            if r in handed:
                continue
            graph[s].add(r)          # an escaping function pointer: its taker may call it
            if fr and not dn(r).startswith("core::ops::function::"):
                addr_rows.add(("EDGE", dn(r), f"{fr[0]}:{fr[1]}"))
        for v in U.vt_refs.get(s, ()):
            if v not in rt_vtables:
                graph[s].update(x for _, x in U.vtables.get(v, ()))
        for g in U.g_refs.get(s, ()):
            graph[s].update(gclose(g))
    roots_cfg = load_roots(a.roots)
    rt_roots, lw_roots, stop_rt, stop_lw = set(handed), set(), set(), set()
    root_hits = collections.Counter()
    for s in U.defined:
        n = dn(s)
        if RUNTIME_TRAITS.search(n):
            rt_roots.add(s)
        for kind, rx, raw, _ in roots_cfg:
            if rx.search(n):
                root_hits[(kind, raw)] += 1
                {"RUNTIME": rt_roots, "LOWERING": lw_roots, "STOP-RUNTIME": stop_rt, "STOP-LOWERING": stop_lw}[kind].add(s)

    def reach(roots, stop):
        seen, st = set(roots), list(roots)
        while st:
            x = st.pop()
            for y in graph.get(x, ()):
                if y not in seen and y in U.defined and y not in stop:
                    seen.add(y)
                    st.append(y)
        return seen
    rt, lw = reach(rt_roots, stop_rt), reach(lw_roots, stop_lw)
    for want in a.why:
        for roots_, stop_, kind_ in ((rt_roots, stop_rt, "runtime"), (lw_roots, stop_lw, "lowering")):
            par, st, hit = {r: None for r in roots_}, collections.deque(roots_), None
            while st and hit is None:
                x = st.popleft()
                if want in dn(x):
                    hit = x
                    break
                for y in graph.get(x, ()):
                    if y not in par and y in U.defined and y not in stop_:
                        par[y] = x
                        st.append(y)
            chain = []
            while hit is not None:
                chain.append(dn(hit))
                hit = par[hit]
            print(f"why {want!r} ({kind_}): " + (" <- ".join(chain) if chain else "not reached"))
    if a.callers:
        rgraph = collections.defaultdict(set)
        for x, ys in graph.items():
            for y in ys:
                rgraph[y].add(x)
        for want in a.callers:
            hits = [x for x in U.defined if want in dn(x)][:3]
            for h in hits:
                print(f"callers of {dn(h)}:")
                seen, frontier = {h}, [h]
                for lvl in range(6):
                    nxt = []
                    for x in frontier:
                        for c in sorted(rgraph.get(x, ()), key=dn)[:8]:
                            if c not in seen:
                                seen.add(c)
                                nxt.append(c)
                                tag = " [RT-ROOT]" if c in rt_roots else (" [LW-ROOT]" if c in lw_roots else "")
                                print("  " * (lvl + 1) + f"<- {dn(c)}{tag}  (of {dn(x)[:60]})")
                    frontier = nxt
    origin, st = {}, collections.deque()
    for r in sorted(rt_roots, key=dn):
        origin[r] = r
        st.append(r)
    while st:
        x = st.popleft()
        for y in graph.get(x, ()):
            if y not in origin and y in U.defined and y not in stop_rt:
                origin[y] = origin[x]
                st.append(y)
    phase = {}
    for s in crate_fn:
        phase[s] = ("BOTH" if s in lw else "RUNTIME") if s in rt else ("LOWERING" if s in lw else "UNREACHED")
        if phase[s] == "UNREACHED" and crate_fn[s] in server_rt:
            phase[s] = "RUNTIME"
    # the run path's calls into a plan-role crate's functions (scope.tsv role PLAN / BOTH crates other than the caller's):
    # each is plan work on the run path (a STOP-RUNTIME candidate) or a runtime island of that crate
    plan_crates = {d for d, _, role in scope if role in ("PLAN", "BOTH")} - {"crates/gravity/driver", "crates/gravity/exec",
                                                                          "crates/gravity/hosts", "crates/gravity/fn"}
    xrows = collections.Counter()
    for s in rt:
        if s not in crate_fn or s in stop_rt:
            continue
        for y in graph.get(s, ()):
            if y in crate_fn and crate_fn[y] != crate_fn[s] and crate_fn[y] in plan_crates and y not in rt_roots and y not in stop_rt:
                xrows[(crate_fn[s], dn(s), crate_fn[y], dn(y))] += 1
    with open(os.path.join(a.out, "rt_into_plan.tsv"), "w") as o:
        o.write("caller_crate\tcaller\tcallee_crate\tcallee\n")
        for k in sorted(xrows):
            o.write("\t".join(k) + "\n")

    # ---- the paths (D4432 q09 W8, HASHAGG-W8 hash aggregates): the operator families of the runtime roots that reach a function
    def fam_of(s):
        return family(U.fn_file[s], crate_fn[s]) if s in crate_fn else None
    by_fam = collections.defaultdict(set)
    for r in rt_roots:
        f_ = fam_of(r)
        if f_:
            by_fam[f_].add(r)
    reached_by = collections.defaultdict(set)
    holders = {x for v in fns_at.values() for x in v}       # + the non-crate functions crate lines were inlined into
    for f_, roots_ in by_fam.items():
        for x in reach(roots_, stop_rt):
            if x in crate_fn or x in holders:
                reached_by[x].add(f_)
    paths_cfg = load_roots(a.paths)    # path<TAB>family regex<TAB>why

    def paths_of(s):
        fams = reached_by.get(s, set())
        ops = {x for x in fams if ":" not in x}     # an operator family; `x:y` = shared machinery / helpers
        hit = [p for p, rx, _, _ in paths_cfg if any(rx.fullmatch(x) for x in ops)]
        if hit:
            return ",".join(hit)
        if fams - ops:
            return "shared"          # reached from shared machinery (the driver run loop, parallel, pool, wire, the runner,
            #                          expression evaluation, generated code's hosts): the graph cannot tie it to one
            #                          operator, so it may lie on any query's path, the named paths included
        if ops:
            return "other"           # reached only from operator families outside every named path
        return "-"

    # ---- inventory: one row per finding, plus one per IR-only miss ----
    by_file_ranges = collections.defaultdict(list)
    for x in crate_fn:
        f, lo, hi = fn_range[x]
        by_file_ranges[f].append((hi - lo, lo, hi, x))
    for f in by_file_ranges:
        by_file_ranges[f].sort()
    fn_src_re = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s+(\w+)")

    def enclosing(f, l):
        best, members = None, []
        for span, lo, hi, x in by_file_ranges.get(f, ()):
            if lo <= l <= hi:
                if best is None:
                    best = (span, x)
                if span == best[0]:
                    members.append(x)
        if best:
            return dn(best[1]), members
        for i in range(l, 0, -1):
            t = src_line(f, i)
            m = fn_src_re.match(t)
            if m:
                return m.group(1) + " (source)", []
            m = re.match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(\w+)", t)
            if m:
                return "derive of " + m.group(1) + " (source)", []
        return "?", []

    def phase_of(ps):
        if not ps:
            return "?"
        if "BOTH" in ps or ("RUNTIME" in ps and "LOWERING" in ps):
            return "BOTH"
        return sorted(ps)[0] if len(ps) == 1 else ("RUNTIME" if "RUNTIME" in ps else sorted(ps)[0])
    sizes = {}
    if a.sizes and os.path.exists(a.sizes):
        for ln in open(a.sizes):
            if ln.startswith("#") or not ln.strip():
                continue
            c = ln.rstrip("\n").split("\t")
            if len(c) >= 3 and c[1].isdigit():
                sizes[(c[0], int(c[1]))] = (c[2], c[3] if len(c) > 3 else "")
    TYPE_OF_ITEM = [(r"std::vec::Vec|^std::vec$|slice::(to_vec|into_vec|concat|join|repeat)|Iterator::(unzip|partition)", "Vec"),
                    (r"std::string::String|^std::format$|fmt::format|ToString::to_string|^str::", "String"),
                    (r"std::boxed::Box", "Box"), (r"std::rc::Rc", "Rc"), (r"std::sync::Arc", "Arc"),
                    (r"HashMap", "HashMap"), (r"HashSet", "HashSet"), (r"BTreeMap", "BTreeMap"), (r"BTreeSet", "BTreeSet"),
                    (r"VecDeque", "VecDeque"), (r"BinaryHeap", "BinaryHeap"), (r"LinkedList", "LinkedList"),
                    (r"CString", "CString"), (r"slice::sort", "sort scratch (Vec)"), (r"thread::", "thread")]
    TYPE_OF_CALLEE = [(r"alloc::string::|alloc::fmt::format|alloc::str::|ToString", "String"),
                      (r"hashbrown|std::collections::hash", "HashMap/HashSet"), (r"btree", "BTreeMap/BTreeSet"),
                      (r"vec_deque", "VecDeque"), (r"binary_heap", "BinaryHeap"), (r"alloc::rc::", "Rc"),
                      (r"alloc::sync::", "Arc"), (r"alloc::boxed::|exchange_malloc|box_new", "Box"),
                      (r"alloc::vec::|raw_vec|alloc::slice::|SpecFromIter|alloc::collections::", "Vec"),
                      (r"^gravity_|^types_|^fires|^mcx", "dependency value")]
    tmemo = {}

    def type_of_sym(sym, depth=0):
        if sym in tmemo:
            return tmemo[sym]
        tmemo[sym] = set()
        n = dn(sym)
        for pat, t in TYPE_OF_CALLEE:
            if re.search(pat, n):
                tmemo[sym] = {t}
                return tmemo[sym]
        out = set()
        if depth < 10:
            for callee, _, _, _ in U.calls.get(sym, ()):
                if callee is not None and callee not in crate_fn and allocates(callee) in ("ENTRY", "ALLOC"):
                    out |= type_of_sym(callee, depth + 1)
        tmemo[sym] = out
        return out

    def alloc_type(kind, item, f, l):
        if kind != "ir-only":
            for pat, t in TYPE_OF_ITEM:
                if re.search(pat, item):
                    return t
        out = set()
        if kind == "ir-only":
            for pat, t in TYPE_OF_CALLEE:
                if re.search(pat, item):
                    out.add(t)
                    break
        for c in line_callees.get((f, l), ()):
            out |= {"dependency value"} if c.startswith("dep:") else type_of_sym(c)
        return "+".join(sorted(out)) if out else ("-" if kind != "ir-only" else "?")
    RE_TAG = re.compile(r"//\s*alloc:\s*(\w+)")

    def tag_of(f, l):
        for i in stmt_window(f, l):
            m = RE_TAG.search(src_line(f, i))
            if m:
                return m.group(1)
        return "-"

    def row(f, l, kind, item, ir_):
        fn, members = enclosing(f, l)
        if not members:
            # a function with no body of its own in the IR (inline(always), a derive): its phase is that of the functions
            # its sites were inlined into
            near = stmt_window(f, l) + [x for x in range(l - 3, l) if x > 0]
            members = sorted(set().union(*(fns_at.get((f, x), set()) for x in near)))
        ph = phase_of({phase.get(x) or (("BOTH" if x in lw else "RUNTIME") if x in rt else ("LOWERING" if x in lw else "UNREACHED"))
                       for x in members})
        if ph in ("UNREACHED", "?") and findings_mod.crate_dir(f, list(crates)) in server_rt:
            ph = "RUNTIME"   # (a line with no body in the union, e.g. a generic instantiated only by its outside caller)
        es = sorted({dn(origin[x]) for x in members if x in origin})
        entry = "-" if not es else es[0] + (f" (+{len(es) - 1})" if len(es) > 1 else "")
        pth = sorted({paths_of(x) for x in members if phase[x] in ("RUNTIME", "BOTH")} - {"-"})
        named = sorted({p for x in pth for p in x.split(",")} - {"shared", "other"})
        pth = ",".join(named) if named else ("shared" if "shared" in pth else ("other" if "other" in pth else "-"))
        cold = homes.cold(f, l, item) or "-"
        if (f, l, item) in refc_at:
            sz = "REFCOUNT-CLONE"   # an Rc / Arc / Weak clone (alone on its line, or beside others: -INLINE): a NAMED class,
            #                         not debt (USER GO via main 2026-10-15T11:46:31Z; USER RULING via memacct 18:5xZ)
        elif cold == "COLD-TEXT":
            sz = "COLD-TEXT"     # Cold-tagged error / refusal / log text: a NAMED class, not debt, not reviewed (USER RULING
            #                      via main 2026-10-14T18:52:43Z); MISTAGGED / COLD-UNPROVEN rows keep their size class
        elif kind == "type":
            sz = "TYPE-ONLY"     # a container type named (a field, a signature): no allocation on the line (was 'HOLDER',
            #                      which read as a holder's charge: it is not one)
        elif ir_ == "BLIND":
            sz = "IR-BLIND"
        elif (f, l) in sizes:
            sz = sizes[(f, l)][0]
        elif ph in ("LOWERING", "UNREACHED"):
            sz = "BOUNDED"
        elif ir_ == "NOALLOC":
            sz = "NONE"
        else:
            sz = "UNREVIEWED"
        c = findings_mod.crate_dir(f, list(crates))
        return (c, f, l, fn, kind, item, alloc_type(kind, item, f, l), ph, family(f, c), sz, ir_, entry, pth, tag_of(f, l), cold)
    inv = [row(f, l, kind, item, ir_of[(f, l)]) for f, l, col, kind, item in findings]
    for x in misses:
        line_callees.setdefault((x[0], x[1]), {x[6]})
        inv.append(row(x[0], x[1], "ir-only", x[4], "ALLOC"))
    inv.sort()

    # ---- outputs ----
    def w(name, rows, head):
        with open(os.path.join(a.out, name), "w") as o:
            o.write("\t".join(head) + "\n")
            for r in rows:
                o.write("\t".join(str(x) for x in r) + "\n")
    cd = lambda f: findings_mod.crate_dir(f, list(crates)) or "?"  # noqa: E731
    w("ir_sites.tsv", sorted((cd(x[0]),) + x[:5] + (phase.get(x[5], "?"),) for x in ir_sites), ["crate", "file", "line", "frame_fn", "fn", "callee", "phase"])
    w("misses.tsv", sorted(x[:5] + (phase.get(x[5], "?"), cd(x[0])) for x in misses), ["file", "line", "frame_fn", "fn", "callee", "phase", "crate"])
    w("fn_misses.tsv", sorted(fmiss), ["file", "line", "fn", "ir_sites", "first_callee"])
    w("home_sites.tsv", sorted(x[:5] + (x[7], phase.get(x[5], "?")) for x in home_sites), ["file", "line", "frame_fn", "fn", "callee", "home", "phase"])
    w("home_findings.tsv", sorted(home_findings), ["file", "line", "col", "kind", "item", "home"])
    w("dep_sites.tsv", sorted(x[:5] + (phase.get(x[5], "?"),) for x in dep_sites), ["file", "line", "frame_fn", "fn", "callee", "phase"])
    w("findings.tsv", [x + (ir_of[(x[0], x[1])],) for x in findings], ["file", "line", "col", "kind", "item", "ir"])
    # NOALLOC (TD-197 LINT-FP, memacct GO 2026-10-15T20:3xZ; findings.NOALLOC_ITEMS): a Clone / ToOwned / Extend finding
    # outside a home whose statement window holds no IR allocation site (ir NOALLOC: inside a function the IR has, no
    # global allocation there); a line the IR does not hold (BLIND) or that allocates (ALLOC) stays debt
    na = collections.Counter()
    for f, l, c, k, it in findings:
        if it in findings_mod.NOALLOC_ITEMS and ir_of[(f, l)] == "NOALLOC" and (f, l, it) not in refc_at:
            # HOMED (memacct 2026-10-15T21:1xZ, planprod's decompose ArenaVec copies): the statement allocates, but only through a
            # home (a Vec<_, ArenaRef> / PgVec / ChargedVec clone, extend or collect_in): owned by construction, not debt.
            # D4672 (memacct 2026-10-16T03:1xZ): judged over the finding's OWN statement, the window the NOALLOC proof (ir_of)
            # reads (noalloc_class): the fixed l-3 .. l+11 reached into the next statements
            na[(f, l, k, it, noalloc_class(homed_at, src_line, f, l))] += 1
        # COLD-TEXT by TYPE (memacct 2026-10-15T20:4xZ, memlead 20:31:22Z (b)): a Cold-tagged Box::new (a plain Cold tag on a
        # non-text item reads MISTAGGED) whose every Box::new instance the IR calls on that line boxes a PgError, the type
        # read from the callee's debug name (new<types_error::pg_error::PgError>; legacy mangling drops generic arguments):
        # an error payload, made and freed on the error path, the COLD-TEXT family
        elif it == findings_mod.BOX_NEW and homes.cold(f, l, it) == "MISTAGGED":
            # the Box::new instance: called (its own debug name), or inlined around box_new_uninit / exchange_malloc (the
            # innermost inlined frame's name); every allocating call of the line must be such a Box<PgError>
            cs_ = callees_at.get((f, l), ())
            titles = [U.sp_title.get(cs, "") if findings_mod.RE_BOX_NEW.match(dn(cs)) else
                      (next(iter(U.inl_title[(f, l, cs)])) if len(U.inl_title.get((f, l, cs), ())) == 1 else "")
                      for cs in cs_]
            if cs_ and all(t in findings_mod.ERR_PAYLOAD_TITLES for t in titles):
                na[(f, l, k, it, "COLD-TEXT")] += 1
    w("noalloc.tsv", sorted((f, l, k, it, n, findings_mod.line_key(src_line(f, l)), cls) for (f, l, k, it, cls), n in na.items()),
      ["file", "line", "kind", "item", "count", "text", "class"])
    w("fn_ranges.tsv", sorted((fn_range[x][0], fn_range[x][1], fn_range[x][2], dn(x), phase[x], ",".join(sorted(reached_by.get(x, ()))))
                              for x in crate_fn), ["file", "lo", "hi", "fn", "phase", "root_families"])
    w("ext_callees.tsv", sorted(((k[1], v, k[0]) for k, v in ext_seen.items()), key=lambda x: (x[0], -x[1])), ["class", "calls", "callee"])
    w("dep_calls.tsv", sorted(((v, k) for k, v in dep_calls.items()), reverse=True), ["calls", "callee\tclass"])
    w("unresolved.tsv", sorted((phase.get(x[0], "non-crate"), x[1], x[2], dn(x[0]), x[3]) for x in unres_rows),
      ["phase", "file", "line", "fn", "vtable_offset"])
    rt_dep = collections.Counter(c for x, c in dep_unknown if phase.get(x) in ("RUNTIME", "BOTH"))
    w("runtime_dep_calls.tsv", sorted(((v, k) for k, v in rt_dep.items()), reverse=True), ["calls", "dependency callee (body not in the union)"])
    w("INVENTORY.tsv", inv, ["crate", "file", "line", "function", "kind", "item", "alloc_type", "phase", "family", "size_class", "ir",
                           "runtime_entry", "paths", "tag", "cold"])
    w("addr_taken.tsv", sorted(addr_rows), ["class", "fn", "where"])
    w("roots.tsv", sorted([("RUNTIME", dn(s), U.fn_file.get(s, "-"), "handed" if s in handed else "trait/cfg") for s in rt_roots]
                          + [("LOWERING", dn(s), U.fn_file.get(s, "-"), "cfg") for s in lw_roots]
                          + [("STOP-RUNTIME", dn(s), U.fn_file.get(s, "-"), "cfg") for s in stop_rt]),
      ["phase", "root", "file", "why"])
    w("root_cfg_hits.tsv", [(k, raw, root_hits.get((k, raw), 0), why) for k, _, raw, why in roots_cfg], ["kind", "regex", "functions", "why"])
    w("homes_status.tsv", [("missing-file",) + x for x in homes.missing] + [("no-such-fn",) + x for x in homes.unmatched],
      ["status", "file", "function", "home"])
    pc = collections.Counter((crate_fn[s], phase[s]) for s in crate_fn)
    w("phase_by_crate.tsv", sorted((c, p, n) for (c, p), n in pc.items()), ["crate", "phase", "functions"])
    alloc_rows = [r for r in inv if r[4] != "type" and r[10] == "ALLOC"]
    cnt = collections.defaultdict(collections.Counter)
    for r in alloc_rows:
        cnt[(r[0], r[7])][r[9]] += 1
    szs = sorted({k for c in cnt.values() for k in c})
    w("counts.tsv", sorted((c, p, sum(v.values())) + tuple(v.get(k, 0) for k in szs) for (c, p), v in cnt.items()),
      ["crate", "phase", "allocating_rows"] + szs)
    fcnt = collections.defaultdict(collections.Counter)
    for r in alloc_rows:
        fcnt[(r[0], r[7], r[8])][r[9]] += 1
    w("counts_family.tsv", sorted((c, p, fm, sum(v.values())) + tuple(v.get(k, 0) for k in szs) for (c, p, fm), v in fcnt.items()),
      ["crate", "phase", "family", "allocating_rows"] + szs)
    # the rows a person must size (RUNTIME / BOTH allocating, not in size_class.tsv), one per line, with the code
    rv = collections.OrderedDict()
    for r in inv:
        if r[9] == "UNREVIEWED":
            k = (r[1], r[2])
            if k not in rv:
                rv[k] = [r[0], r[1], r[2], r[3], set(), r[6], r[7], r[8], r[11], r[12], r[13], set()]
            rv[k][4].add(r[5])
            if r[14] != "-":
                rv[k][11].add(r[14])
    w("review.tsv", [v[:4] + [";".join(sorted(v[4]))] + v[5:11] + [src_line(v[1], v[2]).strip()[:220], ";".join(sorted(v[11])) or "-"]
                     for v in rv.values()],
      ["crate", "file", "line", "function", "items", "alloc_type", "phase", "family", "runtime_entry", "paths", "tag", "code", "cold"])
    tagc = collections.Counter((r[13], r[7]) for r in inv if r[13] != "-" and r[4] != "type")
    w("tags_vs_phase.tsv", sorted((t, p, n) for (t, p), n in tagc.items()), ["alloc_tag", "phase", "rows"])
    with open(os.path.join(a.out, "summary.txt"), "w") as o:
        o.write(f"modules {U.modules} ({len(files)} .ll); crates {len(crates)}; crate functions in IR {len(crate_fn)} "
                f"({len(U.macro_fns)} counted by body, {len(U.host_fns)} host! fns); defined {len(U.defined)}; vtables {len(U.vtables)}\n")
        o.write(f"IR sites: {len(ir_sites)} ({len(set((x[0], x[1]) for x in ir_sites))} lines); covered {covered}; MISSES {len(misses)}"
                f" ({len(set((x[0], x[1]) for x in misses))} lines); FUNCTION MISSES {len(fmiss)}; in homes {len(home_sites)} IR sites, "
                f"{len(home_findings)} findings\n")
        o.write(f"clippy findings: {len(findings)} on {len(by_line)} lines; by IR: {dict(collections.Counter(ir_of.values()))} (lines)\n")
        o.write(f"dep callees: {len(dep_calls)} distinct, {sum(dep_calls.values())} calls; dependency calls that allocate (in the union): "
                f"{len(dep_sites)} sites on {len(set((x[0], x[1]) for x in dep_sites))} lines\n")
        o.write(f"phase: {dict(collections.Counter(phase.values()))}; runtime roots {len(rt_roots)} (handed {len(handed)}, host refs {host_refs}); "
                f"lowering roots {len(lw_roots)}; stops {len(stop_rt)} / {len(stop_lw)}; unresolved indirect calls {unresolved}\n")
        zero = [raw for k, _, raw, _ in roots_cfg if not root_hits.get((k, raw))]
        o.write(f"roots.tsv regexes matching no function: {len(zero)} {zero[:20]}\n")
        o.write(f"homes.tsv: {len(homes.rows)} rows; missing files {len(homes.missing)}; functions not found {len(homes.unmatched)} {homes.unmatched[:10]}\n")
        o.write(f"runtime calls into dependency functions whose bodies the union does not hold: {sum(rt_dep.values())} calls to {len(rt_dep)} functions\n")
        cc = {k: (sum(1 for r in inv if r[14] == k), len({(r[1], r[2]) for r in inv if r[14] == k}))
              for k in ("COLD-TEXT", "COLD-UNPROVEN", "MISTAGGED")}
        o.write("Cold tags (USER RULING 2026-10-14T18:52:43Z): " + "; ".join(f"{k} {n} rows on {ln} lines" for k, (n, ln) in cc.items())
                + " (COLD-TEXT is a named class out of the debt; COLD-UNPROVEN and MISTAGGED stay debt)\n")
        o.write(f"inventory rows: {len(inv)}; allocating (non-type, IR ALLOC) rows {len(alloc_rows)}; by phase x size: "
                f"{dict(collections.Counter((r[7], r[9]) for r in alloc_rows))}\n")
        o.write("misses by crate: " + str(dict(collections.Counter(cd(x[0]) for x in misses))) + "\n")
    print(open(os.path.join(a.out, "summary.txt")).read(), end="")


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        selftest()
    else:
        main()
