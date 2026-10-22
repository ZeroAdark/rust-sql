use alloc::rc::Rc;
use alloc::string::String;
use core::any::Any;

use ::mcx::PgVec;
use ::types_core::PgWChar;

pub type pg_regoff_t = i64;

/// Offsets are character (not byte) positions; `-1` means "no match for this
/// subexpression".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegMatch {
    pub rm_so: pg_regoff_t,
    pub rm_eo: pg_regoff_t,
}

impl RegMatch {
    pub const UNSET: RegMatch = RegMatch { rm_so: -1, rm_eo: -1 };
}

#[derive(Clone)]
pub struct RegexCompiled {
    pub engine: Rc<dyn Any>,
    pub re_nsub: usize,
}

impl core::fmt::Debug for RegexCompiled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegexCompiled")
            .field("re_nsub", &self.re_nsub)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegexFailure {
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum RegcompResult {
    Compiled(RegexCompiled),
    Failed(RegexFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegexecResult {
    Matched,
    NoMatch,
    Failed(RegexFailure),
}

#[derive(Debug)]
pub enum RegprefixResult<'mcx> {
    NoMatch,
    Prefix(PgVec<'mcx, PgWChar>),
    Exact(PgVec<'mcx, PgWChar>),
    Failed(RegexFailure),
}

pub const REG_BASIC: i32 = 0o0;
pub const REG_EXTENDED: i32 = 0o1;
pub const REG_ADVF: i32 = 0o2;
pub const REG_ADVANCED: i32 = 0o3;
pub const REG_QUOTE: i32 = 0o4;
pub const REG_NOSPEC: i32 = REG_QUOTE;
pub const REG_ICASE: i32 = 0o10;
pub const REG_NOSUB: i32 = 0o20;
pub const REG_EXPANDED: i32 = 0o40;
pub const REG_NLSTOP: i32 = 0o100;
pub const REG_NLANCH: i32 = 0o200;
pub const REG_NEWLINE: i32 = 0o300;

pub fn cflags_valid(cflags: i32) -> bool {
    !((cflags & REG_QUOTE) != 0 && (cflags & (REG_ADVANCED | REG_EXPANDED | REG_NEWLINE)) != 0)
        && !((cflags & REG_EXTENDED) == 0 && (cflags & REG_ADVF) != 0)
}
pub const REG_PEND: i32 = 0o400;
pub const REG_EXPECT: i32 = 0o1000;
pub const REG_BOSONLY: i32 = 0o2000;
