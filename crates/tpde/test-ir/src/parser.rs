// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{Block, Function, Name, Op, TestIR, Value, ValueType};
use alloc::{boxed::Box, format};
use core::hash::BuildHasher;
use mcx::{ArenaForget, Mcx, PgFxHashMap};
use tpde_util::SmallVec;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[cold]
pub(crate) fn error(message: impl Into<alloc::string::String>) -> Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}
pub(crate) fn index(value: usize) -> PgResult<u32> {
    u32::try_from(value).map_err(|_| error("TestIR index exceeds u32"))
}
fn input_name(source: &str, name: Name) -> &str {
    &source[name.offset as usize..name.offset as usize + name.len as usize]
}
fn name_offset(source: &str, name: &str) -> PgResult<Name> {
    Ok(Name {
        offset: if name.is_empty() {
            0
        } else {
            index(name.as_ptr().addr() - source.as_ptr().addr())?
        },
        len: index(name.len())?,
    })
}
#[derive(Clone, Copy)]
struct Symbol {
    name: Name,
    value: u32,
    next: u32,
}
mcx::forget_safe_nodrop!(Symbol);
struct NameMap<'m> {
    heads: ArenaForget<PgFxHashMap<'m, u64, u32>>,
    symbols: SmallVec<'m, Symbol, 0>,
}
impl<'m> NameMap<'m> {
    fn new(mcx: Mcx<'m>) -> Self {
        Self {
            heads: ArenaForget::new(PgFxHashMap::with_hasher_in(Default::default(), mcx)),
            symbols: SmallVec::new(mcx),
        }
    }
    fn find(&self, source: &str, name: &str, hash: u64) -> Option<usize> {
        let mut slot = *self.heads.get(&hash)?;
        while slot != u32::MAX {
            let symbol = self.symbols[slot as usize];
            if input_name(source, symbol.name) == name {
                return Some(slot as usize);
            }
            slot = symbol.next;
        }
        None
    }
    fn get(&self, source: &str, name: &str) -> Option<u32> {
        self.find(source, name, self.heads.hasher().hash_one(name))
            .map(|idx| self.symbols[idx].value)
    }
    fn insert(&mut self, mcx: Mcx<'m>, source: &str, name: &str, value: u32) -> PgResult<()> {
        let hash = self.heads.hasher().hash_one(name);
        if let Some(idx) = self.find(source, name, hash) {
            self.symbols[idx].value = value;
            return Ok(());
        }
        self.heads
            .try_reserve(1)
            .map_err(|_| mcx.oom((self.heads.len() + 1).saturating_mul(32)))?;
        let slot = index(self.symbols.len())?;
        self.symbols.push(Symbol {
            name: name_offset(source, name)?,
            value,
            next: self.heads.get(&hash).copied().unwrap_or(u32::MAX),
        })?;
        self.heads.insert(hash, slot);
        Ok(())
    }
    fn clear(&mut self) {
        self.heads.clear();
        self.symbols.clear();
    }
    fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
    fn release(&mut self, mcx: Mcx<'m>) {
        *self.heads = PgFxHashMap::with_hasher_in(Default::default(), mcx);
        self.symbols.release();
    }
    fn retained_bytes(&self) -> usize {
        self.heads.capacity() * core::mem::size_of::<(u64, u32)>()
            + self.symbols.capacity() * core::mem::size_of::<Symbol>()
    }
}
pub(crate) struct Scratch<'m> {
    funcs: NameMap<'m>,
    blocks: NameMap<'m>,
    values: NameMap<'m>,
    func_resolves: SmallVec<'m, (Name, u32), 0>,
    block_resolves: SmallVec<'m, (Name, u32), 0>,
    value_resolves: SmallVec<'m, (Name, u32), 0>,
    phi_blocks: SmallVec<'m, Name, 0>,
}
impl<'m> Scratch<'m> {
    pub(crate) fn new(mcx: Mcx<'m>) -> Self {
        Self {
            funcs: NameMap::new(mcx),
            blocks: NameMap::new(mcx),
            values: NameMap::new(mcx),
            func_resolves: SmallVec::new(mcx),
            block_resolves: SmallVec::new(mcx),
            value_resolves: SmallVec::new(mcx),
            phi_blocks: SmallVec::new(mcx),
        }
    }
    fn clear(&mut self) {
        self.funcs.clear();
        self.blocks.clear();
        self.values.clear();
        self.func_resolves.clear();
        self.block_resolves.clear();
        self.value_resolves.clear();
        self.phi_blocks.clear();
    }
    pub(crate) fn release(&mut self, mcx: Mcx<'m>) {
        self.funcs.release(mcx);
        self.blocks.release(mcx);
        self.values.release(mcx);
        self.func_resolves.release();
        self.block_resolves.release();
        self.value_resolves.release();
        self.phi_blocks.release();
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.funcs.retained_bytes()
            + self.blocks.retained_bytes()
            + self.values.retained_bytes()
            + (self.func_resolves.capacity()
                + self.block_resolves.capacity()
                + self.value_resolves.capacity())
                * core::mem::size_of::<(Name, u32)>()
            + self.phi_blocks.capacity() * core::mem::size_of::<Name>()
    }
}
const _: () = assert!(!core::mem::needs_drop::<Scratch<'static>>());
struct Parser<'a, 'm, 't> {
    source: &'t str,
    text: &'t str,
    ir: &'a mut TestIR<'m>,
    scratch: Scratch<'m>,
}

pub(crate) fn parse(ir: &mut TestIR<'_>, text: &str) -> PgResult<()> {
    let saved = (
        ir.values.len(),
        ir.value_operands.len(),
        ir.blocks.len(),
        ir.functions.len(),
        ir.names.len(),
    );
    let mcx = ir.mcx;
    let scratch = core::mem::replace(&mut ir.scratch, Scratch::new(mcx));
    let mut parser = Parser {
        source: text,
        text,
        ir,
        scratch,
    };
    let result = parser.parse();
    parser.scratch.clear();
    if result.is_err() {
        parser.ir.values.truncate(saved.0);
        parser.ir.value_operands.truncate(saved.1);
        parser.ir.blocks.truncate(saved.2);
        parser.ir.functions.truncate(saved.3);
        parser.ir.names.truncate(saved.4);
    }
    parser.ir.scratch = parser.scratch;
    result
}

impl<'m, 't> Parser<'_, 'm, 't> {
    fn skip_whitespace(&mut self, newlines: bool) {
        while let Some(&byte) = self.text.as_bytes().first() {
            if byte == b';' {
                self.text = match self.text.find('\n') {
                    Some(end) => &self.text[end..],
                    None => "",
                };
            } else if matches!(byte, b'\t'..=b'\r' | b' ') {
                if byte == b'\n' && !newlines {
                    break;
                }
                self.text = &self.text[1..];
            } else {
                break;
            }
        }
    }
    fn try_read(&mut self, byte: u8) -> bool {
        if self.text.as_bytes().first() != Some(&byte) {
            return false;
        }
        self.text = &self.text[1..];
        true
    }
    fn expect(&mut self, byte: u8, message: &'static str) -> PgResult<()> {
        if self.try_read(byte) {
            Ok(())
        } else {
            Err(error(message))
        }
    }
    fn parse_name(&mut self) -> PgResult<&'t str> {
        if !self
            .text
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        {
            return Err(error("unable to parse name"));
        }
        let len = self
            .text
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        let name = &self.text[..len];
        self.text = &self.text[len..];
        Ok(name)
    }
    fn parse_value_name(&mut self) -> PgResult<(&'t str, bool)> {
        self.skip_whitespace(true);
        self.expect(b'%', "expected value name")?;
        let name = self.parse_name()?;
        let force_fixed = self.try_read(b'!');
        Ok((name, force_fixed))
    }
    fn value(&mut self, value_type: ValueType, name: &'t str, fixed: bool) -> PgResult<u32> {
        let idx = index(self.ir.values.len())?;
        self.scratch
            .values
            .insert(self.ir.mcx, self.source, name, idx)?;
        let name = self.ir.add_name(name)?;
        let mut value = Value::new(value_type, name);
        value.force_fixed_assignment = fixed;
        self.ir.values.push(value)?;
        Ok(idx)
    }
    fn unresolved_value(&mut self, name: &'t str) -> PgResult<()> {
        self.scratch.value_resolves.push((
            name_offset(self.source, name)?,
            index(self.ir.value_operands.len())?,
        ))?;
        self.ir.value_operands.push(0)
    }
    fn unresolved_block(&mut self, name: &'t str) -> PgResult<()> {
        self.scratch.block_resolves.push((
            name_offset(self.source, name)?,
            index(self.ir.value_operands.len())?,
        ))?;
        self.ir.value_operands.push(0)
    }
    fn parse_phi(&mut self, name: &'t str, fixed: bool) -> PgResult<()> {
        let val = self.value(ValueType::Phi, name, fixed)? as usize;
        let begin = index(self.ir.value_operands.len())?;
        self.ir.values[val].op_begin_idx = begin;
        self.skip_whitespace(false);
        self.scratch.phi_blocks.clear();
        while !self.text.is_empty() && !self.text.starts_with('\n') {
            if !self.try_read(b'[') {
                break;
            }
            self.expect(b'^', "expected block name")?;
            let block = self.parse_name()?;
            self.scratch
                .phi_blocks
                .push(name_offset(self.source, block)?)?;
            self.skip_whitespace(true);
            self.expect(b',', "expected , in phi tuple")?;
            self.skip_whitespace(true);
            self.expect(b'%', "expected value name")?;
            let operand = self.parse_name()?;
            self.unresolved_value(operand)?;
            self.expect(b']', "missing ]")?;
            if !self.try_read(b',') {
                break;
            }
            self.skip_whitespace(true);
        }
        self.ir.values[val].op_count = index(self.ir.value_operands.len())? - begin;
        for idx in 0..self.scratch.phi_blocks.len() {
            self.unresolved_block(input_name(self.source, self.scratch.phi_blocks[idx]))?;
        }
        self.ir.values[val].op_end_idx = index(self.ir.value_operands.len())?;
        Ok(())
    }
    fn immediate(&mut self, name: &str) -> PgResult<u32> {
        let base = if self.text.starts_with("0x") {
            self.text = &self.text[2..];
            16u32
        } else {
            10
        };
        let mut value = 0u32;
        let mut length = 0;
        for byte in self.text.bytes() {
            let digit = match byte {
                b'0'..=b'9' => (byte - b'0') as u32,
                b'a'..=b'f' => (byte - b'a' + 10) as u32,
                b'A'..=b'F' => (byte - b'A' + 10) as u32,
                _ => break,
            };
            if digit >= base {
                break;
            }
            value = value
                .checked_mul(base)
                .and_then(|v| v.checked_add(digit))
                .ok_or_else(|| error(format!("invalid immediate at '{}'", name)))?;
            length += 1;
        }
        if length == 0 {
            return Err(error(format!("invalid immediate at '{}'", name)));
        }
        self.text = &self.text[length..];
        Ok(value)
    }
    fn parse_inst(&mut self) -> PgResult<()> {
        self.skip_whitespace(true);
        let (name, fixed) = if self.text.starts_with('%') {
            let name = self.parse_value_name()?;
            self.skip_whitespace(true);
            self.expect(b'=', "expected =")?;
            self.skip_whitespace(false);
            name
        } else {
            ("", false)
        };
        let op_name = if self.text.starts_with('%') || self.text.starts_with('\n') {
            "any"
        } else {
            self.parse_name()?
        };
        if op_name == "phi" {
            return self.parse_phi(name, fixed);
        }
        let op = Op::ALL
            .into_iter()
            .find(|op| op.info().name == op_name)
            .ok_or_else(|| error(format!("invalid op '{}'", op_name)))?;
        let info = op.info();
        let value_type = if info.is_terminator {
            ValueType::Terminator
        } else {
            ValueType::Normal
        };
        let val = self.value(value_type, name, fixed)? as usize;
        self.ir.values[val].op = op;
        self.ir.values[val].op_begin_idx = index(self.ir.value_operands.len())?;
        if op == Op::Call {
            self.ir.functions.last_mut().unwrap().has_call = true;
            self.skip_whitespace(false);
            self.expect(b'@', "expected global function after call")?;
            let target = self.parse_name()?;
            self.scratch
                .func_resolves
                .push((name_offset(self.source, target)?, index(val)?))?;
            self.skip_whitespace(false);
            if self.try_read(b',') {
                self.skip_whitespace(true);
            }
        }
        let mut op_idx = 0u32;
        let mut block_begin = u32::MAX;
        let mut imm_begin = u32::MAX;
        self.skip_whitespace(false);
        while !self.text.is_empty() && !self.text.starts_with('\n') {
            if op_idx < block_begin && self.try_read(b'%') {
                let operand = self.parse_name()?;
                self.unresolved_value(operand)?;
            } else if op_idx < imm_begin && self.try_read(b'^') {
                if block_begin == u32::MAX {
                    block_begin = op_idx;
                }
                let operand = self.parse_name()?;
                self.unresolved_block(operand)?;
            } else {
                if imm_begin == u32::MAX {
                    imm_begin = op_idx;
                }
                let immediate = self.immediate(name)?;
                self.ir.value_operands.push(immediate)?;
            }
            op_idx = op_idx
                .checked_add(1)
                .ok_or_else(|| error("operand count exceeds u32"))?;
            self.skip_whitespace(false);
            if !self.try_read(b',') {
                break;
            }
            self.skip_whitespace(true);
        }
        if imm_begin == u32::MAX {
            imm_begin = op_idx;
        }
        if block_begin == u32::MAX {
            block_begin = imm_begin;
        }
        self.ir.values[val].op_count = block_begin;
        for (kind, expected, got) in [
            ("operand", info.op_count, block_begin),
            ("block", info.succ_count, imm_begin - block_begin),
            ("imm", info.imm_count, op_idx - imm_begin),
        ] {
            if expected != u32::MAX && expected != got {
                return Err(error(format!(
                    "{} count mismatch for {}: expected {} got {}",
                    kind, op_name, expected, got
                )));
            }
        }
        if info.is_terminator {
            let begin = self.ir.values[val].op_begin_idx;
            let block = self.ir.blocks.last_mut().unwrap();
            block.succ_begin_idx = begin + block_begin;
            block.succ_end_idx = begin + imm_begin;
        }
        self.ir.values[val].op_end_idx = index(self.ir.value_operands.len())?;
        Ok(())
    }
    fn parse_block(&mut self) -> PgResult<()> {
        self.skip_whitespace(true);
        let name = self.parse_name().unwrap_or("");
        self.expect(b':', "expected :")?;
        if self.scratch.blocks.get(self.source, name).is_some() {
            return Err(error(format!("duplicate block name {}", name)));
        }
        self.scratch
            .blocks
            .insert(self.ir.mcx, self.source, name, index(self.ir.blocks.len())?)?;
        let name = self.ir.add_name(name)?;
        let begin = index(self.ir.values.len())?;
        self.ir.blocks.push(Block {
            name,
            inst_begin_idx: begin,
            phi_end_idx: begin,
            ..Block::default()
        })?;
        loop {
            self.parse_inst()?;
            if self.ir.values.last().unwrap().value_type == ValueType::Phi {
                let end = index(self.ir.values.len())?;
                let block = self.ir.blocks.last_mut().unwrap();
                if block.phi_end_idx != end - 1 {
                    return Err(error("phis must be at beginning of block"));
                }
                block.phi_end_idx = end;
            }
            if self.ir.values.last().unwrap().value_type == ValueType::Terminator {
                break;
            }
        }
        self.ir.blocks.last_mut().unwrap().inst_end_idx = index(self.ir.values.len())?;
        Ok(())
    }
    fn parse_func(&mut self) -> PgResult<()> {
        self.scratch.blocks.clear();
        self.scratch.values.clear();
        self.scratch.block_resolves.clear();
        self.scratch.value_resolves.clear();
        self.skip_whitespace(true);
        let mut name = self.parse_name()?;
        let mut is_declaration = false;
        if name == "define" || name == "declare" {
            is_declaration = name == "declare";
            self.skip_whitespace(true);
            self.expect(
                b'@',
                "expected @ after LLVM-IR-compatible function definition",
            )?;
            name = self.parse_name().unwrap_or("");
        }
        if self.scratch.funcs.get(self.source, name).is_some() {
            return Err(error(format!("Duplicate function {}", name)));
        }
        let idx = index(self.ir.functions.len())?;
        let stored_name = self.ir.add_name(name)?;
        self.ir.functions.push(Function {
            name: stored_name,
            arg_begin_idx: index(self.ir.values.len())?,
            ..Function::default()
        })?;
        self.scratch
            .funcs
            .insert(self.ir.mcx, self.source, name, idx)?;
        self.skip_whitespace(true);
        self.expect(b'(', "expected (")?;
        loop {
            self.skip_whitespace(true);
            if self.try_read(b')') {
                break;
            }
            if !self.scratch.values.is_empty() {
                self.expect(b',', "expected comma")?;
                self.skip_whitespace(true);
            }
            let (arg, fixed) = self.parse_value_name()?;
            self.value(ValueType::Arg, arg, fixed)?;
        }
        self.ir.functions.last_mut().unwrap().arg_end_idx = index(self.ir.values.len())?;
        self.skip_whitespace(true);
        if is_declaration || self.try_read(b'!') {
            self.ir.functions.last_mut().unwrap().declaration = true;
            return Ok(());
        }
        if self.text.starts_with("local") {
            self.ir.functions.last_mut().unwrap().local_only = true;
            self.text = &self.text[5..];
        }
        self.skip_whitespace(true);
        self.expect(b'{', "expected {")?;
        self.ir.functions.last_mut().unwrap().block_begin_idx = index(self.ir.blocks.len())?;
        loop {
            self.skip_whitespace(true);
            if self.try_read(b'}') {
                break;
            }
            self.parse_block()?;
        }
        self.ir.functions.last_mut().unwrap().block_end_idx = index(self.ir.blocks.len())?;
        for &(name, idx) in &*self.scratch.block_resolves {
            let name = input_name(self.source, name);
            self.ir.value_operands[idx as usize] = self
                .scratch
                .blocks
                .get(self.source, name)
                .ok_or_else(|| error(format!("use of undefined block {}", name)))?;
        }
        for &(name, idx) in &*self.scratch.value_resolves {
            let name = input_name(self.source, name);
            self.ir.value_operands[idx as usize] = self
                .scratch
                .values
                .get(self.source, name)
                .ok_or_else(|| error(format!("use of undefined value {}", name)))?;
        }
        Ok(())
    }
    fn parse(&mut self) -> PgResult<()> {
        loop {
            self.skip_whitespace(true);
            if self.text.is_empty() {
                break;
            }
            self.parse_func()?;
        }
        for &(name, idx) in &*self.scratch.func_resolves {
            let name = input_name(self.source, name);
            let target = self
                .scratch
                .funcs
                .get(self.source, name)
                .ok_or_else(|| error(format!("use of undefined function {}", name)))?;
            let value = &mut self.ir.values[idx as usize];
            value.call_func_idx = target;
            let function = self.ir.functions[target as usize];
            let argc = function.arg_end_idx - function.arg_begin_idx;
            if value.op_count != argc {
                return Err(error(format!(
                    "arg count mismatch in call {}, expected {} got {}",
                    name, argc, value.op_count
                )));
            }
        }
        Ok(())
    }
}
