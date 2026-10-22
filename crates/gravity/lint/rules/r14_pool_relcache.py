"""R14: a pool-side file reaches the relcache or the catalog. Code that runs on a pool thread has no relcache, no syscache
and no catalog snapshot of its own: a Relation, a syscache probe, an fmgr_info lookup or a relcache buffer read there
reads (or builds) per-backend state from a thread that does not own it. The btree read path a pool thread runs goes
through an IndexImage the leader captured, bound per thread as an ImageRel (nbtree imagerel.rs): buffers by locator
(read_buffer_without_relcache), catalog answers from the image's frozen tables, functions by fmgr_isbuiltin. The rule
reads a fixed list of pool-only files (POOL_SIDE), outside the crates the per-file rules walk; a listed file that is gone
fails too (when its crate is there), so a rename cannot switch the rule off."""
import re
from common import Src

ID = "R14"
TITLE = "a pool-side file names a relcache or catalog API"
INSTEAD = ("read through the leader's captured image: ImageRel (buffers by locator via read_buffer_without_relcache, "
           "catalog answers from the image's tables, functions via fmgr_isbuiltin)")
HOME = []
GLOBAL = True

POOL_SIDE = [
    "crates/backend/access/nbtree/nbtree/src/imagerel.rs",
    "crates/gravity/hosts/src/ops/scan/bitmap_leaves.rs",
]

_API = re.compile(
    r"(?<![A-Za-z0-9_])(?:Relation|RelationData|relation_open|index_open|table_open|RelationGetDescr|RelationIdGetRelation"
    r"|lsyscache|syscache|syscache_seams|SearchSysCache\w*|get_opfamily_proc|get_opfamily_member|get_opcode"
    r"|get_typlenbyvalalign|read_buffer::call|ReadBuffer(?!Mode)\w*|predicate_lock\w*)(?![A-Za-z0-9_])"
    r"|(?<![A-Za-z0-9_])fmgr_info(?:_cxt)?\s*\("
    r"|\.rd_[a-z]\w*")


def _not_definition(t, m):
    """A method definition (`fn fmgr_info(`, `fn predicate_lock_page(`) names the trait's own method, which the image
    answers without the catalog: only calls and other uses count."""
    return not re.search(r"\bfn\s+$", t[max(0, m.start() - 8):m.start()])


def check_tree(ctx):
    """-> [(path, line, note)]: every relcache / catalog name in a pool-side file's code (comments are free)."""
    out = []
    for p in POOL_SIDE:
        t = ctx.read(p)
        if t is None:
            if ctx.read(p.split("/src/")[0] + "/Cargo.toml") is None:   # a tree without the file's crate
                continue
            out.append((p, 1, "a pool-side file R14 lists is gone: update POOL_SIDE in the rule"))
            continue
        src = Src(p, t)
        for ln in src.hits(_API, keep=_not_definition):
            out.append((p, ln, f"{TITLE}; instead: {INSTEAD} | {src.lines[ln - 1].strip()[:120]}"))
    return out


PLANTED_TOML = (POOL_SIDE[0],
                "pub fn tupdesc<'a>(r: &'a ImageRel<'_, '_>, rel: &Relation<'_>) -> usize {\n"
                "    rel.rd_att.natts as usize\n}\n")
CLEAN_TOML = (POOL_SIDE[0],
              "// reads no Relation: the image answers from its own tables\n"
              "fn fmgr_info(&self, proc: Oid) -> PgResult<FmgrInfo> { self.resolve(proc) }\n"
              "fn predicate_lock_page(&self, blk: u32) -> PgResult<()> { Ok(()) }\n"
              "pub fn natts(r: &ImageRel<'_, '_>) -> usize {\n    r.img.natts as usize\n}\n")
