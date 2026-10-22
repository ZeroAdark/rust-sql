use core::fmt::Write;

/// A pointer-sized field of a block the generated code reads and writes as ONE 64-bit word (abi.md: "abi C-unwind i64";
/// gravity_wasm loads every `Ptr` as `i64`). On a 64-bit host it IS the pointer (`P` itself: the native layout, the
/// native code and the IR pins are untouched). On wasm32 it is the 4-byte pointer in the low half of an 8-aligned word
/// whose high half stays zero, so the same offsets and the same 8-byte loads and stores hold (Datum's own wasm32 shape,
/// datum.rs). Read with [`PtrWord::ptr`], write with [`word`].
#[cfg(target_pointer_width = "64")]
pub type Word<P> = P;

/// See the 64-bit `Word`: the wasm32 slot, the pointer in the low half, a zero high half.
#[cfg(not(target_pointer_width = "64"))]
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Word<P: Copy> {
    p: P,
    hi: u32,
}

#[cfg(not(target_pointer_width = "64"))]
const _: () = assert!(core::mem::size_of::<Word<*mut u8>>() == 8 && core::mem::size_of::<Word<Option<unsafe extern "C" fn()>>>() == 8);

/// `p` as a block's pointer word (the identity on a 64-bit host).
#[cfg(target_pointer_width = "64")]
#[inline(always)]
pub const fn word<P: Copy>(p: P) -> Word<P> {
    p
}

/// `p` as a block's pointer word: the low half, a zero high half.
#[cfg(not(target_pointer_width = "64"))]
#[inline(always)]
pub const fn word<P: Copy>(p: P) -> Word<P> {
    Word { p, hi: 0 }
}

/// The pointer in a block's pointer word, in a `const fn` (the identity on a 64-bit host).
#[cfg(target_pointer_width = "64")]
#[inline(always)]
pub const fn unword<P: Copy>(w: Word<P>) -> P {
    w
}

/// The pointer in a block's pointer word, in a `const fn`: its low half.
#[cfg(not(target_pointer_width = "64"))]
#[inline(always)]
pub const fn unword<P: Copy>(w: Word<P>) -> P {
    w.p
}

/// The pointer in a block's pointer word (the identity on a 64-bit host).
pub trait PtrWord: Copy {
    type P: Copy;
    fn ptr(self) -> Self::P;
}

#[cfg(target_pointer_width = "64")]
impl<P: Copy> PtrWord for P {
    type P = P;
    #[inline(always)]
    fn ptr(self) -> P {
        self
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl<P: Copy> PtrWord for Word<P> {
    type P = P;
    #[inline(always)]
    fn ptr(self) -> P {
        self.p
    }
}

/// One `#[repr(C)]` block and its offset table from one definition: the struct, a module of
/// offset constants (`offset_of!` per field), `FIELDS` as data for the lowerer and the golden,
/// and the size / alignment pins. Every field named here is one load or store at a fixed offset.
#[macro_export]
macro_rules! block {
    (
        $(#[$m:meta])* $name:ident size $size:literal offsets $offs:ident
        { $( $(#[$fm:meta])* $field:ident => $C:ident : $ty:ty ),* $(,)? }
        $( extra { $($extra:item)* } )?
    ) => {
        $(#[$m])*
        #[repr(C)]
        pub struct $name { $( $(#[$fm])* pub $field: $ty, )* }
        pub mod $offs {
            #[allow(unused_imports)]
            use super::*;
            $( pub const $C: usize = ::core::mem::offset_of!($name, $field); )*
            pub const SIZE: usize = $size;
            pub const FIELDS: &[(&str, usize, usize)] = &[ $( (stringify!($field), $C, ::core::mem::size_of::<$ty>()) ),* ];
            $( $($extra)* )?
        }
        const _: () = assert!(::core::mem::size_of::<$name>() == $size && ::core::mem::align_of::<$name>() == 8);
    };
}

/// The whole layout as data: `(block, size, fields)`, fields as `(name, offset, size)`.
pub const LAYOUT: &[(&str, usize, &[(&str, usize, usize)])] = &[
    ("ctx", crate::ctx::hdr::SIZE, crate::ctx::hdr::FIELDS),
    ("site", crate::site::site::SIZE, crate::site::site::FIELDS),
    ("program", crate::program::prog::SIZE, crate::program::prog::FIELDS),
    ("out", crate::ctx::out::SIZE, crate::ctx::out::FIELDS),
    ("lane", crate::ctx::lane::SIZE, crate::ctx::lane::FIELDS),
    ("answer", crate::ctx::answer::SIZE, crate::ctx::answer::FIELDS),
    ("partials", crate::ctx::partials::SIZE, crate::ctx::partials::FIELDS),
    ("identity", crate::ctx::identity::SIZE, crate::ctx::identity::FIELDS),
    ("scanpos", crate::ctx::scanpos::SIZE, crate::ctx::scanpos::FIELDS),
];

pub fn describe(out: &mut dyn Write) -> core::fmt::Result {
    use crate::ctx::{hdr, lane};
    use crate::site::site;
    use crate::status;
    writeln!(out, "gravity_abi ctx layout v4 (abi-v2)")?;
    for (block, size, fields) in LAYOUT {
        writeln!(out, "{block} size {size} align 8")?;
        for (name, off, sz) in fields.iter() {
            writeln!(out, "{block} {name} {off} {sz}")?;
        }
    }
    writeln!(out, "ctx claim.lo {} 8 claim.hi {} 8", hdr::CLAIM_LO, hdr::CLAIM_HI)?;
    writeln!(out, "site pos.flags {} 2 pos.off {} 2 pos.rel {} 4 pos.block {} 4", site::POS_FLAGS, site::POS_OFF, site::POS_REL, site::POS_BLOCK)?;
    writeln!(out, "site window cells {} {}", site::WINDOW_CELLS.0, site::WINDOW_CELLS.1)?;
    writeln!(out, "sites stride {} count ctx.nsites after header {}", site::SIZE, hdr::SIZE)?;
    writeln!(out, "lanes count site.nlanes per program")?;
    writeln!(out, "lane kind byval 0 byref 1 codes 2 fixed 3")?;
    writeln!(out, "lane region page 0 arena 1 dictionary 2 breaker 3 immediate 4")?;
    writeln!(out, "lane flags all_valid 1 sorted 2 verdict mixed 0 all_match 4 none_match 8 answered 16 entries 32")?;
    writeln!(out, "lane id kind<<24 attr 0 rowid 1 offset 2 verdict 3 projection 4 jsonb 5 table 6")?;
    writeln!(out, "lane empty_code_none {}", lane::EMPTY_CODE_NONE)?;
    writeln!(out, "answer flags sum 1 minmax 2")?;
    writeln!(out, "site state closed 0 open 1 eof 2")?;
    writeln!(out, "pos flags valid 1 row 2")?;
    writeln!(out, "flags resume 1")?;
    writeln!(out, "claim all 0 {}", u64::MAX)?;
    writeln!(out, "resume phase<<48|position none 0 build 1 probe 2 spill_batch 3 emit 4")?;
    writeln!(out, "status ok 0 stop {} cancelled {} error {} suspend -({}|full<<8) recheck -({}|site<<8) unsupported -({}|kind<<8) fault -({}|site<<8|lane<<16|code<<24)", status::STOP, status::CANCELLED, status::ERROR, -status::SUSPEND, -status::RECHECK, -status::UNSUPPORTED, -status::FAULT)?;
    writeln!(out, "unsupported shape 1 scratch 2 null_key 3 on_worker 4 out_region 5")?;
    writeln!(out, "write op insert 1 update 2 delete 3 value skipped 0 done 1 recheck 2")?;
    writeln!(out, "spill op scatter 0 refill 1 parts 2 drop 3 target 4 run_put 5 runs 6 run_open 7 run_fill 8")?;
    writeln!(out, "calls site next(site) lane(site,k) fetch(site,nrows) program push(ctx) spill(ctx,op,arg) arena_grow(ctx,bytes) write(ctx,op,site) lock(ctx,site) rec_open(ctx,partial,layout,cap,limit,keys)")?;
    writeln!(out, "abi C-unwind i64")
}
