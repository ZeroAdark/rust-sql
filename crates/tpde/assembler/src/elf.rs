// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![allow(non_camel_case_types)]
use tpde_util::tpde_assert_eq;

macro_rules! constants {
    ($group:ident, $ty:ty; $($name:ident = $value:expr),* $(,)?) => {
        $(pub const $name: $ty = $value;)*
        #[cfg(feature = "upstream-oracle")]
        pub const $group: &[(&str, u64)] = &[$((stringify!($name), $name as u64)),*];
    };
}
constants!(IDENT, i32;
    EI_MAG0 = 0,
    EI_MAG1 = 1,
    EI_MAG2 = 2,
    EI_MAG3 = 3,
    EI_CLASS = 4,
    EI_DATA = 5,
    EI_VERSION = 6,
    EI_OSABI = 7,
    EI_ABIVERSION = 8,
    EI_PAD = 9,
    EI_NIDENT = 16
);
pub const ELFMAG: &[u8; 5] = b"\x7fELF\0";
constants!(HALF, u16;
    ET_NONE = 0,
    ET_REL = 1,
    ET_EXEC = 2,
    ET_DYN = 3,
    ET_CORE = 4,
    EM_X86_64 = 62,
    EM_AARCH64 = 183,
    SHN_UNDEF = 0,
    SHN_LORESERVE = 0xff00,
    SHN_LOPROC = 0xff00,
    SHN_HIPROC = 0xff1f,
    SHN_LOOS = 0xff20,
    SHN_HIOS = 0xff3f,
    SHN_ABS = 0xfff1,
    SHN_COMMON = 0xfff2,
    SHN_XINDEX = 0xffff,
    SHN_HIRESERVE = 0xffff
);
constants!(BYTE, u8;
    ELFCLASSNONE = 0,
    ELFCLASS32 = 1,
    ELFCLASS64 = 2,
    ELFDATANONE = 0,
    ELFDATA2LSB = 1,
    ELFDATA2MSB = 2,
    ELFOSABI_SYSV = 0,
    STB_LOCAL = 0,
    STB_GLOBAL = 1,
    STB_WEAK = 2,
    STB_LOOS = 10,
    STB_HIOS = 12,
    STB_LOPROC = 13,
    STB_HIPROC = 15,
    STT_NOTYPE = 0,
    STT_OBJECT = 1,
    STT_FUNC = 2,
    STT_SECTION = 3,
    STT_FILE = 4,
    STT_COMMON = 5,
    STT_TLS = 6,
    STT_LOOS = 10,
    STT_GNU_IFUNC = 10,
    STT_HIOS = 12,
    STT_LOPROC = 13,
    STT_HIPROC = 15,
    STV_DEFAULT = 0,
    STV_INTERNAL = 1,
    STV_HIDDEN = 2,
    STV_PROTECTED = 3
);
constants!(WORD, u32;
    EV_NONE = 0,
    EV_CURRENT = 1,
    SHT_NULL = 0,
    SHT_PROGBITS = 1,
    SHT_SYMTAB = 2,
    SHT_STRTAB = 3,
    SHT_RELA = 4,
    SHT_HASH = 5,
    SHT_DYNAMIC = 6,
    SHT_NOTE = 7,
    SHT_NOBITS = 8,
    SHT_REL = 9,
    SHT_SHLIB = 10,
    SHT_DYNSYM = 11,
    SHT_INIT_ARRAY = 14,
    SHT_FINI_ARRAY = 15,
    SHT_PREINIT_ARRAY = 16,
    SHT_GROUP = 17,
    SHT_SYMTAB_SHNDX = 18,
    SHT_X86_64_UNWIND = 0x70000001,
    SHF_WRITE = 1<<0,
    SHF_ALLOC = 1<<1,
    SHF_EXECINSTR = 1<<2,
    SHF_MERGE = 1<<4,
    SHF_STRINGS = 1<<5,
    SHF_INFO_LINK = 1<<6,
    SHF_LINK_ORDER = 1<<7,
    SHF_OS_NONCONFORMING = 1<<8,
    SHF_GROUP = 1<<9,
    SHF_TLS = 1<<10,
    SHF_COMPRESSED = 1<<11,
    SHF_GNU_RETAIN = 1<<21,
    SHF_EXCLUDE = 1<<31,
    GRP_COMDAT = 1,
    GRP_MASKOS = 0x0ff00000,
    GRP_MASKPROC = 0xf0000000
);
constants!(X64, u32;
    R_X86_64_NONE = 0,
    R_X86_64_64 = 1,
    R_X86_64_PC32 = 2,
    R_X86_64_GOT32 = 3,
    R_X86_64_PLT32 = 4,
    R_X86_64_COPY = 5,
    R_X86_64_GLOB_DAT = 6,
    R_X86_64_JUMP_SLOT = 7,
    R_X86_64_RELATIVE = 8,
    R_X86_64_GOTPCREL = 9,
    R_X86_64_32 = 10,
    R_X86_64_32S = 11,
    R_X86_64_16 = 12,
    R_X86_64_PC16 = 13,
    R_X86_64_8 = 14,
    R_X86_64_PC8 = 15,
    R_X86_64_DTPMOD64 = 16,
    R_X86_64_DTPOFF64 = 17,
    R_X86_64_TPOFF64 = 18,
    R_X86_64_TLSGD = 19,
    R_X86_64_TLSLD = 20,
    R_X86_64_DTPOFF32 = 21,
    R_X86_64_GOTTPOFF = 22,
    R_X86_64_TPOFF32 = 23,
    R_X86_64_PC64 = 24,
    R_X86_64_GOTOFF64 = 25,
    R_X86_64_GOTPC32 = 26,
    R_X86_64_GOT64 = 27,
    R_X86_64_GOTPCREL64 = 28,
    R_X86_64_GOTPC64 = 29,
    R_X86_64_GOTPLT64 = 30,
    R_X86_64_PLTOFF64 = 31,
    R_X86_64_SIZE32 = 32,
    R_X86_64_SIZE64 = 33,
    R_X86_64_GOTPC32_TLSDESC = 34,
    R_X86_64_TLSDESC_CALL = 35,
    R_X86_64_TLSDESC = 36,
    R_X86_64_IRELATIVE = 37,
    R_X86_64_GOTPCRELX = 41,
    R_X86_64_REX_GOTPCRELX = 42,
    R_X86_64_CODE_4_GOTPCRELX = 43,
    R_X86_64_CODE_4_GOTTPOFF = 44,
    R_X86_64_CODE_4_GOTPC32_TLSDESC = 45,
    R_X86_64_CODE_6_GOTTPOFF = 50
);
constants!(A64, u32;
    R_AARCH64_NONE = 0,
    R_AARCH64_ABS64 = 257,
    R_AARCH64_ABS32 = 258,
    R_AARCH64_ABS16 = 259,
    R_AARCH64_PREL64 = 260,
    R_AARCH64_PREL32 = 261,
    R_AARCH64_PREL16 = 262,
    R_AARCH64_MOVW_UABS_G0 = 263,
    R_AARCH64_MOVW_UABS_G0_NC = 264,
    R_AARCH64_MOVW_UABS_G1 = 265,
    R_AARCH64_MOVW_UABS_G1_NC = 266,
    R_AARCH64_MOVW_UABS_G2 = 267,
    R_AARCH64_MOVW_UABS_G2_NC = 268,
    R_AARCH64_MOVW_UABS_G3 = 269,
    R_AARCH64_MOVW_SABS_G0 = 270,
    R_AARCH64_MOVW_SABS_G1 = 271,
    R_AARCH64_MOVW_SABS_G2 = 272,
    R_AARCH64_LD_PREL_LO19 = 273,
    R_AARCH64_ADR_PREL_LO21 = 274,
    R_AARCH64_ADR_PREL_PG_HI21 = 275,
    R_AARCH64_ADR_PREL_PG_HI21_NC = 276,
    R_AARCH64_ADD_ABS_LO12_NC = 277,
    R_AARCH64_LDST8_ABS_LO12_NC = 278,
    R_AARCH64_TSTBR14 = 279,
    R_AARCH64_CONDBR19 = 280,
    R_AARCH64_JUMP26 = 282,
    R_AARCH64_CALL26 = 283,
    R_AARCH64_LDST16_ABS_LO12_NC = 284,
    R_AARCH64_LDST32_ABS_LO12_NC = 285,
    R_AARCH64_LDST64_ABS_LO12_NC = 286,
    R_AARCH64_MOVW_PREL_G0 = 287,
    R_AARCH64_MOVW_PREL_G0_NC = 288,
    R_AARCH64_MOVW_PREL_G1 = 289,
    R_AARCH64_MOVW_PREL_G1_NC = 290,
    R_AARCH64_MOVW_PREL_G2 = 291,
    R_AARCH64_MOVW_PREL_G2_NC = 292,
    R_AARCH64_MOVW_PREL_G3 = 293,
    R_AARCH64_LDST128_ABS_LO12_NC = 299,
    R_AARCH64_MOVW_GOTOFF_G0 = 300,
    R_AARCH64_MOVW_GOTOFF_G0_NC = 301,
    R_AARCH64_MOVW_GOTOFF_G1 = 302,
    R_AARCH64_MOVW_GOTOFF_G1_NC = 303,
    R_AARCH64_MOVW_GOTOFF_G2 = 304,
    R_AARCH64_MOVW_GOTOFF_G2_NC = 305,
    R_AARCH64_MOVW_GOTOFF_G3 = 306,
    R_AARCH64_GOTREL64 = 307,
    R_AARCH64_GOTREL32 = 308,
    R_AARCH64_GOT_LD_PREL19 = 309,
    R_AARCH64_LD64_GOTOFF_LO15 = 310,
    R_AARCH64_ADR_GOT_PAGE = 311,
    R_AARCH64_LD64_GOT_LO12_NC = 312,
    R_AARCH64_LD64_GOTPAGE_LO15 = 313,
    R_AARCH64_TLSGD_ADR_PREL21 = 512,
    R_AARCH64_TLSGD_ADR_PAGE21 = 513,
    R_AARCH64_TLSGD_ADD_LO12_NC = 514,
    R_AARCH64_TLSGD_MOVW_G1 = 515,
    R_AARCH64_TLSGD_MOVW_G0_NC = 516,
    R_AARCH64_TLSLD_ADR_PREL21 = 517,
    R_AARCH64_TLSLD_ADR_PAGE21 = 518,
    R_AARCH64_TLSLD_ADD_LO12_NC = 519,
    R_AARCH64_TLSLD_MOVW_G1 = 520,
    R_AARCH64_TLSLD_MOVW_G0_NC = 521,
    R_AARCH64_TLSLD_LD_PREL19 = 522,
    R_AARCH64_TLSLD_MOVW_DTPREL_G2 = 523,
    R_AARCH64_TLSLD_MOVW_DTPREL_G1 = 524,
    R_AARCH64_TLSLD_MOVW_DTPREL_G1_NC = 525,
    R_AARCH64_TLSLD_MOVW_DTPREL_G0 = 526,
    R_AARCH64_TLSLD_MOVW_DTPREL_G0_NC = 527,
    R_AARCH64_TLSLD_ADD_DTPREL_HI12 = 528,
    R_AARCH64_TLSLD_ADD_DTPREL_LO12 = 529,
    R_AARCH64_TLSLD_ADD_DTPREL_LO12_NC = 530,
    R_AARCH64_TLSLD_LDST8_DTPREL_LO12 = 531,
    R_AARCH64_TLSLD_LDST8_DTPREL_LO12_NC = 532,
    R_AARCH64_TLSLD_LDST16_DTPREL_LO12 = 533,
    R_AARCH64_TLSLD_LDST16_DTPREL_LO12_NC = 534,
    R_AARCH64_TLSLD_LDST32_DTPREL_LO12 = 535,
    R_AARCH64_TLSLD_LDST32_DTPREL_LO12_NC = 536,
    R_AARCH64_TLSLD_LDST64_DTPREL_LO12 = 537,
    R_AARCH64_TLSLD_LDST64_DTPREL_LO12_NC = 538,
    R_AARCH64_TLSIE_MOVW_GOTTPREL_G1 = 539,
    R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC = 540,
    R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 = 541,
    R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC = 542,
    R_AARCH64_TLSIE_LD_GOTTPREL_PREL19 = 543,
    R_AARCH64_TLSLE_MOVW_TPREL_G2 = 544,
    R_AARCH64_TLSLE_MOVW_TPREL_G1 = 545,
    R_AARCH64_TLSLE_MOVW_TPREL_G1_NC = 546,
    R_AARCH64_TLSLE_MOVW_TPREL_G0 = 547,
    R_AARCH64_TLSLE_MOVW_TPREL_G0_NC = 548,
    R_AARCH64_TLSLE_ADD_TPREL_HI12 = 549,
    R_AARCH64_TLSLE_ADD_TPREL_LO12 = 550,
    R_AARCH64_TLSLE_ADD_TPREL_LO12_NC = 551,
    R_AARCH64_TLSLE_LDST8_TPREL_LO12 = 552,
    R_AARCH64_TLSLE_LDST8_TPREL_LO12_NC = 553,
    R_AARCH64_TLSLE_LDST16_TPREL_LO12 = 554,
    R_AARCH64_TLSLE_LDST16_TPREL_LO12_NC = 555,
    R_AARCH64_TLSLE_LDST32_TPREL_LO12 = 556,
    R_AARCH64_TLSLE_LDST32_TPREL_LO12_NC = 557,
    R_AARCH64_TLSLE_LDST64_TPREL_LO12 = 558,
    R_AARCH64_TLSLE_LDST64_TPREL_LO12_NC = 559,
    R_AARCH64_TLSDESC_LD_PREL19 = 560,
    R_AARCH64_TLSDESC_ADR_PREL21 = 561,
    R_AARCH64_TLSDESC_ADR_PAGE21 = 562,
    R_AARCH64_TLSDESC_LD64_LO12 = 563,
    R_AARCH64_TLSDESC_ADD_LO12 = 564,
    R_AARCH64_TLSDESC_OFF_G1 = 565,
    R_AARCH64_TLSDESC_OFF_G0_NC = 566,
    R_AARCH64_TLSDESC_LDR = 567,
    R_AARCH64_TLSDESC_ADD = 568,
    R_AARCH64_TLSDESC_CALL = 569,
    R_AARCH64_TLSLE_LDST128_TPREL_LO12 = 570,
    R_AARCH64_TLSLE_LDST128_TPREL_LO12_NC = 571,
    R_AARCH64_TLSLD_LDST128_DTPREL_LO12 = 572,
    R_AARCH64_TLSLD_LDST128_DTPREL_LO12_NC = 573,
    R_AARCH64_COPY = 1024,
    R_AARCH64_GLOB_DAT = 1025,
    R_AARCH64_JUMP_SLOT = 1026,
    R_AARCH64_RELATIVE = 1027,
    R_AARCH64_TLS_DTPMOD = 1028,
    R_AARCH64_TLS_DTPREL = 1029,
    R_AARCH64_TLS_TPREL = 1030,
    R_AARCH64_TLSDESC = 1031,
    R_AARCH64_IRELATIVE = 1032
);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Elf64_Ehdr {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Elf64_Shdr {
    pub sh_name: u32,
    pub sh_type: u32,
    pub sh_flags: u64,
    pub sh_addr: u64,
    pub sh_offset: u64,
    pub sh_size: u64,
    pub sh_link: u32,
    pub sh_info: u32,
    pub sh_addralign: u64,
    pub sh_entsize: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Elf64_Sym {
    pub st_name: u32,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: u16,
    pub st_value: u64,
    pub st_size: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Elf64_Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64,
}
impl Elf64_Sym {
    pub const fn st_bind(&self) -> u8 {
        self.st_info >> 4
    }
    pub const fn st_type(&self) -> u8 {
        self.st_info & 15
    }
}
pub const fn st_info(binding: u32, kind: u32) -> u32 {
    (binding << 4).wrapping_add(kind & 15)
}
pub const fn r_info(symbol: u64, kind: u32) -> u64 {
    (symbol << 32).wrapping_add(kind as u64)
}

macro_rules! encode {
    ($name:ident, $size:expr; $($field:ident),*) => {
        impl $name {
            pub fn to_le_bytes(&self) -> [u8; $size] {
                let mut out = [0; $size];
                let mut offset = 0;
                $(let bytes = self.$field.to_le_bytes();
                  out[offset..offset + bytes.len()].copy_from_slice(&bytes);
                  offset += bytes.len();)*
                tpde_assert_eq!(offset, $size);
                out
            }
        }
    };
}
encode!(Elf64_Shdr, 64; sh_name, sh_type, sh_flags, sh_addr, sh_offset, sh_size, sh_link, sh_info, sh_addralign, sh_entsize);
encode!(Elf64_Sym, 24; st_name, st_info, st_other, st_shndx, st_value, st_size);
encode!(Elf64_Rela, 24; r_offset, r_info, r_addend);
impl Elf64_Ehdr {
    pub fn to_le_bytes(&self) -> [u8; 64] {
        let mut out = [0; 64];
        out[..16].copy_from_slice(&self.e_ident);
        let mut off = 16;
        macro_rules! fields {
            ($($f:ident),*) => {
                $(
                    let bytes = self.$f.to_le_bytes();
                    out[off..off + bytes.len()].copy_from_slice(&bytes); off += bytes.len();
                )*
            };
        }
        fields!(
            e_type,
            e_machine,
            e_version,
            e_entry,
            e_phoff,
            e_shoff,
            e_flags,
            e_ehsize,
            e_phentsize,
            e_phnum,
            e_shentsize,
            e_shnum,
            e_shstrndx
        );
        tpde_assert_eq!(off, 64);
        out
    }
}
const _: () = {
    assert!(core::mem::size_of::<Elf64_Ehdr>() == 64);
    assert!(core::mem::size_of::<Elf64_Shdr>() == 64);
    assert!(core::mem::size_of::<Elf64_Sym>() == 24);
    assert!(core::mem::size_of::<Elf64_Rela>() == 24);
};
