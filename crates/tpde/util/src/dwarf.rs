// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![allow(non_upper_case_globals)]

macro_rules! constants {
    ($($name:ident: $ty:ty = $value:expr;)*) => {
        $(pub const $name: $ty = $value;)*
        #[cfg(feature = "upstream-oracle")]
        pub const CONSTANTS: &[(&str, u32)] = &[$((stringify!($name), $name as u32)),*];
    };
}
constants! {
    DW_CFA_nop: u8 = 0;
    DW_EH_PE_uleb128: u8 = 0x01;
    DW_EH_PE_pcrel: u8 = 0x10;
    DW_EH_PE_indirect: u8 = 0x80;
    DW_EH_PE_sdata4: u8 = 0x0b;
    DW_EH_PE_omit: u8 = 0xff;
    DW_CFA_advance_loc1: u8 = 0x02;
    DW_CFA_advance_loc2: u8 = 0x03;
    DW_CFA_advance_loc4: u8 = 0x04;
    DW_CFA_offset_extended: u8 = 0x05;
    DW_CFA_restore_extended: u8 = 0x06;
    DW_CFA_undefined: u8 = 0x07;
    DW_CFA_same_value: u8 = 0x08;
    DW_CFA_register: u8 = 0x09;
    DW_CFA_remember_state: u8 = 0x0a;
    DW_CFA_restore_state: u8 = 0x0b;
    DW_CFA_def_cfa: u8 = 0x0c;
    DW_CFA_def_cfa_register: u8 = 0x0d;
    DW_CFA_def_cfa_offset: u8 = 0x0e;
    DW_CFA_advance_loc: u8 = 0x40;
    DW_CFA_offset: u8 = 0x80;
    DW_CFA_restore: u8 = 0xc0;
    DWARF_CFI_PRIMARY_OPCODE_MASK: u8 = 0xc0;
    EH_FDE_FUNC_START_OFF: u32 = 0x8;
}
pub mod x64 {
    constants! {
        DW_reg_rax: u8 = 0;
        DW_reg_rdx: u8 = 1;
        DW_reg_rcx: u8 = 2;
        DW_reg_rbx: u8 = 3;
        DW_reg_rsi: u8 = 4;
        DW_reg_rdi: u8 = 5;
        DW_reg_rbp: u8 = 6;
        DW_reg_rsp: u8 = 7;
        DW_reg_r8: u8 = 8;
        DW_reg_r9: u8 = 9;
        DW_reg_r10: u8 = 10;
        DW_reg_r11: u8 = 11;
        DW_reg_r12: u8 = 12;
        DW_reg_r13: u8 = 13;
        DW_reg_r14: u8 = 14;
        DW_reg_r15: u8 = 15;
        DW_reg_ra: u8 = 16;
    }
}
pub mod a64 {
    constants! {
        DW_reg_x0: u8 = 0;
        DW_reg_x1: u8 = 1;
        DW_reg_x2: u8 = 2;
        DW_reg_x3: u8 = 3;
        DW_reg_x4: u8 = 4;
        DW_reg_x5: u8 = 5;
        DW_reg_x6: u8 = 6;
        DW_reg_x7: u8 = 7;
        DW_reg_x8: u8 = 8;
        DW_reg_x9: u8 = 9;
        DW_reg_x10: u8 = 10;
        DW_reg_x11: u8 = 11;
        DW_reg_x12: u8 = 12;
        DW_reg_x13: u8 = 13;
        DW_reg_x14: u8 = 14;
        DW_reg_x15: u8 = 15;
        DW_reg_x16: u8 = 16;
        DW_reg_x17: u8 = 17;
        DW_reg_x18: u8 = 18;
        DW_reg_x19: u8 = 19;
        DW_reg_x20: u8 = 20;
        DW_reg_x21: u8 = 21;
        DW_reg_x22: u8 = 22;
        DW_reg_x23: u8 = 23;
        DW_reg_x24: u8 = 24;
        DW_reg_x25: u8 = 25;
        DW_reg_x26: u8 = 26;
        DW_reg_x27: u8 = 27;
        DW_reg_x28: u8 = 28;
        DW_reg_x29: u8 = 29;
        DW_reg_x30: u8 = 30;
        DW_reg_fp: u8 = 29;
        DW_reg_lr: u8 = 30;
        DW_reg_v0: u8 = 64;
        DW_reg_v1: u8 = 65;
        DW_reg_v2: u8 = 66;
        DW_reg_v3: u8 = 67;
        DW_reg_v4: u8 = 68;
        DW_reg_v5: u8 = 69;
        DW_reg_v6: u8 = 70;
        DW_reg_v7: u8 = 71;
        DW_reg_v8: u8 = 72;
        DW_reg_v9: u8 = 73;
        DW_reg_v10: u8 = 74;
        DW_reg_v11: u8 = 75;
        DW_reg_v12: u8 = 76;
        DW_reg_v13: u8 = 77;
        DW_reg_v14: u8 = 78;
        DW_reg_v15: u8 = 79;
        DW_reg_v16: u8 = 80;
        DW_reg_v17: u8 = 81;
        DW_reg_v18: u8 = 82;
        DW_reg_v19: u8 = 83;
        DW_reg_v20: u8 = 84;
        DW_reg_v21: u8 = 85;
        DW_reg_v22: u8 = 86;
        DW_reg_v23: u8 = 87;
        DW_reg_v24: u8 = 88;
        DW_reg_v25: u8 = 89;
        DW_reg_v26: u8 = 90;
        DW_reg_v27: u8 = 91;
        DW_reg_v28: u8 = 92;
        DW_reg_v29: u8 = 93;
        DW_reg_v30: u8 = 94;
        DW_reg_sp: u8 = 31;
        DW_reg_pc: u8 = 32;
    }
}
