#![no_std]

use ::mcx::PgBox;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CollProvider {
    Default = b'd',
    Builtin = b'b',
    Icu = b'i',
    Libc = b'c',
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PgLocaleStruct {
    pub provider: CollProvider,
    pub deterministic: bool,
    pub collate_is_c: bool,
    pub ctype_is_c: bool,
    pub is_default: bool,
}

/// `pg_locale_t` is occasionally checked for truth in C, so callers use `Option<PgLocale>` where C uses a possibly-NULL pointer.
pub type PgLocale<'mcx> = PgBox<'mcx, PgLocaleStruct>;
