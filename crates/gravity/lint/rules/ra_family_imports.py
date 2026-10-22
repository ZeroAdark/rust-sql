"""Rule (a) of constructs #31: a family never imports another family's ops/. `*/ops/<f>/` across crates is one family;
a split lane's subdirectory is its node's family. The dispatch (ops/mod.rs), the homes and the substrate are not
families. Families meet through the request, the answer and plan facts. `ops/shared/` is the home of the
op descriptors several families read: any family may import it, and it imports no family itself.

GRANTS: the named items one family may read in another's ops/ (reader, owner) -> {crate: item paths below
`<crate>::ops::<owner>::`}. A grant names items, never a whole family: a path to an item outside the list still hits,
and so does a bare module import. The owner family keeps its list: a change to a listed item's name, signature or
meaning is the owner's to announce to the reader, and a new item needs a ruling. `*` in a path matches any name."""
import fnmatch
import re
from common import crate, families, in_src

ID = "RA"
TITLE = "a family names another family's ops/ module"
INSTEAD = "the other node's answer (frame::iface), plan facts, or a home (a shared op descriptor: ops::shared)"
HOME = []
SHARED = "shared"
SCOPE = in_src
_ABS = re.compile(r"\b(?:crate|gravity_[a-z0-9_]+)::ops::(\{[^}]*\}|[a-z0-9_]+)")
_SUP = re.compile(r"(?<![:\w])((?:super::)+)(\{[^}]*\}|[a-z0-9_]+)")
# a whole path below ops:: (`agg::hashagg::make`, `agg::trans::face::{Acc, FoldClass}`), with its crate word
_FULL = re.compile(r"\b(crate|gravity_[a-z0-9_]+)::ops::((?:[A-Za-z0-9_]+::)*(?:\{[^{}]*\}|[A-Za-z0-9_]+))")

# A GroupAggregate, a Unique and a hashed or sorted DISTINCT are nodeAgg.c's and nodeGroup.c's siblings: group's lowering
# builds its groups with the keyed hash aggregate's maker and accessors, and its hosts fold into the hosted aggregate
# arena. These are the items it reads; the hosted arena's layout words (STATE_*, AGG_STATE_BYTES) go with the arena.
# Each owner family keeps its own list.
GRANTS = {
    ("group", "agg"): {
        "lowerv4": ("hashagg::make", "hashagg::table_decl", "hashagg::flush_win", "hashagg::ex_val", "hashagg::Agg",
                    "hashagg::AccAt", "hashagg::Arg", "hashagg::GroupRec", "hashagg::KeyVal",
                    "trans::plain::class_of", "trans::face::Acc", "trans::face::FoldClass"),
        "decompose": ("trans::table::row",),
        "kernels": ("entries::*", "trans::hosted::DIRECT_*", "trans::hosted::STATE_*", "trans::hosted::AGG_STATE_BYTES"),
        "hosts": ("trans::hosted::AggArena", "trans::hosted::Slot", "trans::hosted::FOLDED",
                  "trans::hosted_kept::image_len"),
    },
    # A streamed group finishes a parted aggregate as the top-N's parted strategy does.
    ("group", "sort"): {
        "lowerv4": ("topn::strategies::parted::finish",),
    },
    # A DISTINCT or ORDER BY aggregate of a sorted group folds host-sorted (D1210): the hosted aggregate bracket binds
    # group's sort spec for it, and agg's plain and hosted ends ask group whether the group's other calls fold in
    # PostgreSQL's row order, whether the group reads its Sort's drain, and where its host-sorted calls may run (D1371).
    # The keyed hash aggregate carries a distinct group's run the way group's plain DISTINCT does (run_carry), and the
    # sorted aggregate's decomposition asks group's presort-absorb whether its keys' Sort folds into the group's input.
    ("agg", "group"): {
        "lowerv4": ("regroup::riders_free", "regroup::call_order", "regroup::presorted", "distinct_plain::run_carry"),
        "hosts": ("hosted_distinct::Sorted", "hosted_distinct::Sorted::of"),
        "decompose": ("absorb::order_free", "absorb::distinct_sort", "absorb::wide_sort", "absorb::key_sort",
                      "absorb::KeySort"),
    },
    # agg's keyed end runs topn's two-pass and parted strategies over its own hash end, and the HAVING admission sizes
    # topn's run area: calls into the top-N's working API, not items a rule-11 move takes. D3153: the generic owner's
    # runs form lays only the groups over its reader's HAVING count floor, twopass.having's floor packed for the owner;
    # its count top's floor (topn lower.rs `top_floor`) where the top is the group's reader.
    ("agg", "sort"): {
        "lowerv4": ("topn::strategies::parted::parted", "topn::strategies::parted::owner",
                    "topn::strategies::twopass::join", "topn::lower::count_top_of",
                    "topn::strategies::twopass_having::owner_floor", "topn::lower::top_floor"),
        "kernels": ("topn::parthaving::runs_area",),
    },
    # agg's pgrc bound reads the pgrc source's run-time API (granule recipe, part list, zone census): not a rule-11 item.
    ("agg", "scan"): {
        "hosts": ("pgrc::granule_source::GranuleRecipe", "pgrc::granule_source::PartList",
                  "pgrc::granule_source::RtFilter", "pgrc::granule_source::zone_census",
                  "pgrc::granule_source::part_extrema", "pgrc::granule_source::ROWS"),
    },
    # An aggregate argument's SubPlans are built through subplan's own map_subplan and breakers: subplan's working API.
    ("agg", "subplan"): {
        "decompose": ("attach_agg_arg",),
    },
}


def _module(path):
    rest = path.split("/src/", 1)[1][:-3].split("/")
    return rest[:-1] if rest[-1] in ("mod", "lib", "main") else rest


def _items(tail):
    """`a::b::{C, D}` -> [`a::b::C`, `a::b::D`]; `{a::B, c}` -> [`a::B`, `c`]."""
    if not tail.endswith("}"):
        return [tail]
    head, _, inner = tail[:-1].partition("{")
    return [head + n.strip() for n in inner.split(",") if n.strip()]


def _granted(own, src_crate, line):
    """The families this line names only through items a grant lists for `own`."""
    if not any(r == own for r, _ in GRANTS):
        return set()
    ok, bad = set(), set()
    for m in _FULL.finditer(line):
        word = m.group(1)
        krate = src_crate if word == "crate" else word[len("gravity_"):]
        for item in _items(m.group(2)):
            fam, _, rest = item.partition("::")
            allowed = GRANTS.get((own, fam), {}).get(krate, ())
            if rest and any(fnmatch.fnmatchcase(rest, p) for p in allowed):
                ok.add(fam)
            else:
                bad.add(fam)
    return ok - bad


def check(src, ctx):
    own = src.family
    if own is None:
        return []
    mod = _module(src.path)
    if "families" not in ctx.cache:
        ctx.cache["families"] = families(ctx.root) | {own}
    fams, out = ctx.cache["families"], []
    src_crate = crate(src.path)
    for i, c in enumerate(src.code):
        names = []
        granted = _granted(own, src_crate, c)
        for m in _ABS.finditer(c):
            names += [n.strip().split("::")[0] for n in m.group(1).strip("{}").split(",")
                      if n.strip().split("::")[0] not in granted]
        for m in _SUP.finditer(c):
            up = m.group(1).count("super::")
            base = mod[:len(mod) - up] if up <= len(mod) else []
            # mod.rs is its directory's module: `super` from ops/agg/mod.rs is ops
            targets = [n.strip().split("::")[0] for n in m.group(2).strip("{}").split(",")]
            for t in targets:
                path = base + [t]
                if len(path) == 2 and path[0] == "ops":
                    names.append(t)
        if any(n and n != own and n != SHARED and n in fams for n in names):
            out.append(i + 1)
    return out


FIXTURES = [("crates/gravity/lowerv4/src/ops/sort/mod.rs", "pub mod runs;\n"),     # the other family, for the selftest
            ("crates/gravity/lowerv4/src/ops/agg/mod.rs", "pub mod hashagg;\n"),  # a granting family
            ("crates/gravity/lowerv4/src/ops/shared/mod.rs", "pub mod sort;\n")]
PLANTED = [("crates/gravity/lowerv4/src/ops/mergejoin/planted.rs", "use crate::ops::sort::runs::RunReader;\n"),
           ("crates/gravity/lowerv4/src/ops/shared/planted.rs", "use crate::ops::sort::runs::RunReader;\n"),
           # an item the grant does not list, a granted item read by another family, a granted item beside another
           # family, a granted item through another crate, and a bare module import
           ("crates/gravity/lowerv4/src/ops/group/planted.rs", "use crate::ops::agg::hashagg::Unlisted;\n"),
           ("crates/gravity/lowerv4/src/ops/mergejoin/planted2.rs", "let t = crate::ops::agg::hashagg::make(px, k);\n"),
           ("crates/gravity/lowerv4/src/ops/group/planted3.rs", "use crate::ops::{agg::hashagg::make, sort::runs};\n"),
           ("crates/gravity/lowerv4/src/ops/group/planted4.rs", "use gravity_hosts::ops::agg::hashagg::make;\n"),
           ("crates/gravity/lowerv4/src/ops/group/planted5.rs", "use crate::ops::agg::hashagg;\n")]
CLEAN = [("crates/gravity/lowerv4/src/ops/mergejoin/clean.rs",
          "use crate::ops::mergejoin::keys::Keys;\nuse crate::frame::iface::Answer;\nuse super::keys;\nuse crate::ops::shared::sort::SortKey;\n"),
         ("crates/gravity/lowerv4/src/ops/group/clean2.rs",
          "use crate::ops::agg::trans::face::{Acc, FoldClass};\nlet t = crate::ops::agg::hashagg::table_decl(px, k, &rec, 0);\n"
          "let f = gravity_kernels::ops::agg::entries::distinct_count_fns();\nuse gravity_decompose::ops::agg::trans::table::row;\n")]
