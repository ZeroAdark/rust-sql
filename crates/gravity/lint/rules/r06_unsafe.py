"""R6: `unsafe` outside the modules allowed it (PLAN-v5 §3.2; constructs #10), and the compiler half: every crate root
carries `#![deny(unsafe_code)]` (or `forbid` where the crate has none), and `allow(unsafe_code)` appears only where
`unsafe` is allowed or on LEGACY. The allowed modules: each family's raw.rs, mem, contract, and the substrate crates' own
modules outside ops/ (common.SUBSTRATE). `#[no_mangle]` and `#[export_name]` count: the compiler's
unsafe_code lint fires on them."""
import re
from common import G, SUBSTRATE, crate_dir, home, in_any, in_src, is_crate_root, is_substrate, word_start

ID = "R6"
TITLE = "unsafe code (or allow(unsafe_code)) outside raw.rs, mem, contract and the substrate"
INSTEAD = "a safe wrapper in the module's raw.rs or raw/ (a family's ops/<family>/raw.rs), mem or contract"
SCOPE = in_src
_HOME = home("raw", "mem", "contract")


def is_home(p):
    return in_any(p, _HOME) or is_substrate(p)


PATTERN = re.compile(r"unsafe\s*(?:\{|fn\b|impl\b|trait\b|extern\b)|allow\(\s*unsafe_code\s*\)"
                     r"|no_mangle\b|export_name\b|link_section\b")
_ROOT = re.compile(r"^\s*#!\[\s*(?:deny|forbid)\(\s*unsafe_code\s*\)\s*\]")


def check(src, ctx):
    return src.hits(PATTERN, keep=word_start) if not is_home(src.path) else []


def check_status(src, ctx):
    """Not LEGACY-able: a crate root outside the substrate carries #![deny(unsafe_code)] (or forbid) unless the root
    itself still holds unsafe (then its directory is on LEGACY by the pattern above). The compiler then refuses unsafe in
    every module without #![allow(unsafe_code)], and the pattern keeps that allow in homes and LEGACY."""
    if (is_crate_root(src.path) and crate_dir(src.path) not in SUBSTRATE
            and not any(_ROOT.match(c) for c in src.code)
            and not any("unsafe_code" not in src.code[n - 1] for n in src.hits(PATTERN, keep=word_start))):
        return [(1, "the crate root lacks #![deny(unsafe_code)] (or #![forbid(unsafe_code)] where the crate has none)")]
    return []


PLANTED = [(G + "hosts/src/ops/window/planted.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n"),
           (G + "driver/src/hostimpl/draw.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n")]
CLEAN = [(G + "hosts/src/ops/window/raw.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n"),
         (G + "driver/src/hostimpl/raw.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n"),
         (G + "backend/wasm/src/raw.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n")]
PLANTED_STATUS = [(G + "fn/src/lib.rs", "pub mod typesem;\n")]
CLEAN_STATUS = [(G + "fn/src/lib.rs", "#![deny(unsafe_code)]\npub mod typesem;\n"),
                (G + "exec/src/lib.rs", "pub fn f(p: *const u8) -> u8 { unsafe { *p } }\n")]
