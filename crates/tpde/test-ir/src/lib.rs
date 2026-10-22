// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
extern crate alloc;

pub mod adaptor;
mod parser;
mod printer;
pub use adaptor::{IRBlockRef, IRFuncRef, IRInstRef, IRValueRef, TestIRAdaptor};
use core::cell::Cell;
use mcx::Mcx;
use tpde_util::SmallVec;
use types_error::PgResult;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Name {
    pub offset: u32,
    pub len: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    Normal,
    Arg,
    Phi,
    Terminator,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    None,
    Any,
    Add,
    Sub,
    Condselect,
    Alloca,
    Terminate,
    Trap,
    Ret,
    Br,
    Condbr,
    Tbz,
    Jump,
    Call,
    Zerofill,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Neq,
    Uge,
    Ugt,
    Ule,
    Ult,
    Sge,
    Sgt,
    Sle,
    Slt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpInfo {
    pub name: &'static str,
    pub is_terminator: bool,
    pub is_def: bool,
    pub op_count: u32,
    pub succ_count: u32,
    pub imm_count: u32,
}
const fn info(name: &'static str, term: bool, def: bool, ops: u32, succ: u32, imm: u32) -> OpInfo {
    OpInfo {
        name,
        is_terminator: term,
        is_def: def,
        op_count: ops,
        succ_count: succ,
        imm_count: imm,
    }
}
pub const OP_INFOS: [OpInfo; 15] = [
    info("<none>", false, false, 0, 0, 0),
    info("any", false, true, u32::MAX, 0, 0),
    info("add", false, true, 2, 0, 0),
    info("sub", false, true, 2, 0, 0),
    info("condselect", false, true, 4, 0, 1),
    info("alloca", false, true, 0, 0, 2),
    info("terminate", true, false, 0, 0, 0),
    info("trap", true, false, 0, 0, 0),
    info("ret", true, false, 1, 0, 0),
    info("br", true, false, 0, 1, 0),
    info("condbr", true, false, 1, 2, 0),
    info("tbz", true, false, 1, 2, 1),
    info("jump", true, false, 0, u32::MAX, 0),
    info("call", false, true, u32::MAX, 0, 0),
    info("zerofill", false, true, 0, 0, 1),
];
impl Op {
    pub const ALL: [Self; 15] = [
        Self::None,
        Self::Any,
        Self::Add,
        Self::Sub,
        Self::Condselect,
        Self::Alloca,
        Self::Terminate,
        Self::Trap,
        Self::Ret,
        Self::Br,
        Self::Condbr,
        Self::Tbz,
        Self::Jump,
        Self::Call,
        Self::Zerofill,
    ];
    pub const fn info(self) -> &'static OpInfo {
        &OP_INFOS[self as usize]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub name: Name,
    pub value_type: ValueType,
    pub op: Op,
    pub force_fixed_assignment: bool,
    pub call_func_idx: u32,
    pub op_count: u32,
    pub op_begin_idx: u32,
    pub op_end_idx: u32,
}
impl Value {
    pub const fn new(value_type: ValueType, name: Name) -> Self {
        Self {
            name,
            value_type,
            op: Op::None,
            force_fixed_assignment: false,
            call_func_idx: u32::MAX,
            op_count: 0,
            op_begin_idx: 0,
            op_end_idx: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Block {
    pub name: Name,
    pub succ_begin_idx: u32,
    pub succ_end_idx: u32,
    pub inst_begin_idx: u32,
    pub phi_end_idx: u32,
    pub inst_end_idx: u32,
    pub block_info: Cell<u32>,
    pub block_info2: Cell<u32>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub name: Name,
    pub declaration: bool,
    pub local_only: bool,
    pub has_call: bool,
    pub block_begin_idx: u32,
    pub block_end_idx: u32,
    pub arg_begin_idx: u32,
    pub arg_end_idx: u32,
}
mcx::forget_safe_nodrop!(Name, ValueType, Op, Cond, Value, Block, Function);
const _: () = {
    assert!(core::mem::size_of::<Value>() <= 128);
    assert!(core::mem::size_of::<Block>() <= 128);
    assert!(core::mem::size_of::<Function>() <= 128);
};

#[doc = include_str!("../tests/ownership.md")]
pub struct TestIR<'m> {
    pub values: SmallVec<'m, Value, 0>,
    pub value_operands: SmallVec<'m, u32, 0>,
    pub blocks: SmallVec<'m, Block, 0>,
    pub functions: SmallVec<'m, Function, 0>,
    names: SmallVec<'m, u8, 0>,
    mcx: Mcx<'m>,
    scratch: parser::Scratch<'m>,
}
impl<'m> TestIR<'m> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            values: SmallVec::new(mcx),
            value_operands: SmallVec::new(mcx),
            blocks: SmallVec::new(mcx),
            functions: SmallVec::new(mcx),
            names: SmallVec::new(mcx),
            mcx,
            scratch: parser::Scratch::new(mcx),
        }
    }
    pub fn name(&self, name: Name) -> &str {
        core::str::from_utf8(
            &self.names[name.offset as usize..name.offset as usize + name.len as usize],
        )
        .expect("TestIR identifiers are ASCII")
    }
    pub fn parse_ir(&mut self, text: &str) -> PgResult<()> {
        parser::parse(self, text)
    }
    pub fn clear(&mut self) {
        self.values.clear();
        self.value_operands.clear();
        self.blocks.clear();
        self.functions.clear();
        self.names.clear();
    }
    pub fn release(&mut self) {
        self.values.release();
        self.value_operands.release();
        self.blocks.release();
        self.functions.release();
        self.names.release();
        self.scratch.release(self.mcx);
    }
    pub fn retained_bytes(&self) -> usize {
        self.values.capacity() * core::mem::size_of::<Value>()
            + self.value_operands.capacity() * 4
            + self.blocks.capacity() * core::mem::size_of::<Block>()
            + self.functions.capacity() * core::mem::size_of::<Function>()
            + self.names.capacity()
            + self.scratch.retained_bytes()
    }
    fn add_name(&mut self, text: &str) -> PgResult<Name> {
        let offset = parser::index(self.names.len())?;
        let len = parser::index(text.len())?;
        parser::index(
            self.names
                .len()
                .checked_add(text.len())
                .ok_or_else(|| parser::error("name storage overflow"))?,
        )?;
        self.names.extend_from_slice(text.as_bytes())?;
        Ok(Name { offset, len })
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
