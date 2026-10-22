// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::Result;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

macro_rules! instruction_flags {
    ($($name:ident: $width:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct InstrFlags {
            $(pub $name: u8,)*
        }

        impl InstrFlags {
            pub fn encode(self) -> u64 {
                let mut result = 0_u64;
                let mut shift = 0;
                $(
                    result |= (u64::from(self.$name) & ((1_u64 << $width) - 1)) << shift;
                    shift += $width;
                )*
                let _ = shift;
                result
            }
        }
    };
}

instruction_flags! {
    modrm_idx: 2,
    modreg_idx: 2,
    vexreg_idx: 2,
    imm_idx: 2,
    evex_bcst: 1,
    evex_mask: 1,
    zeroreg_val: 1,
    lock: 1,
    imm_control: 3,
    vsib: 1,
    modrm_size: 2,
    modreg_size: 2,
    vexreg_size: 2,
    imm_size: 2,
    legacy: 1,
    unused2: 1,
    size_fix1: 3,
    size_fix2: 2,
    instr_width: 1,
    modrm_ty: 3,
    modreg_ty: 3,
    vexreg_ty: 2,
    imm_ty: 0,
    evex_rc: 2,
    evex_bcst16: 1,
    opsize: 3,
    modrm: 1,
    ign66: 1,
}

impl InstrFlags {
    pub fn idx(&self, name: &str) -> u8 {
        match name {
            "modrm" => self.modrm_idx,
            "modreg" => self.modreg_idx,
            "vexreg" => self.vexreg_idx,
            "imm" => self.imm_idx,
            _ => panic!("unknown operand encoding: {name}"),
        }
    }

    fn set_operand(&mut self, name: &str, size: u8, ty: u8) -> Result<()> {
        match name {
            "modrm" => {
                self.modrm_size = size;
                self.modrm_ty = ty;
            }
            "modreg" => {
                self.modreg_size = size;
                self.modreg_ty = ty;
            }
            "vexreg" => {
                self.vexreg_size = size;
                self.vexreg_ty = ty;
            }
            "imm" => {
                self.imm_size = size;
                self.imm_ty = ty;
            }
            _ => return Err(format!("unknown operand encoding: {name}")),
        }
        Ok(())
    }
}

pub fn encoding(name: &str) -> Result<InstrFlags> {
    let mut flags = InstrFlags::default();
    let (modrm, rm, reg, vex, imm, control, zero) = match name {
        "NP" => (0, 0, 0, 0, 0, 0, 0),
        "M" => (1, 3, 0, 0, 0, 0, 0),
        "R" => (1, 0, 3, 0, 0, 0, 0),
        "M1" => (1, 3, 0, 0, 2, 1, 0),
        "MI" => (1, 3, 0, 0, 2, 4, 0),
        "IM" => (1, 2, 0, 0, 3, 4, 0),
        "MC" => (1, 3, 0, 2, 0, 0, 1),
        "MR" => (1, 3, 2, 0, 0, 0, 0),
        "RM" => (1, 2, 3, 0, 0, 0, 0),
        "RMA" => (1, 2, 3, 1, 0, 0, 0),
        "MRI" => (1, 3, 2, 0, 1, 4, 0),
        "RMI" => (1, 2, 3, 0, 1, 4, 0),
        "MRC" => (1, 3, 2, 1, 0, 0, 1),
        "AM" => (1, 2, 0, 3, 0, 0, 0),
        "MA" => (1, 3, 0, 2, 0, 0, 0),
        "I" => (0, 0, 0, 0, 3, 4, 0),
        "IA" => (0, 0, 0, 3, 2, 4, 0),
        "O" => (0, 3, 0, 0, 0, 0, 0),
        "OI" => (0, 3, 0, 0, 2, 4, 0),
        "OA" => (0, 3, 0, 2, 0, 0, 0),
        "S" => (0, 0, 3, 0, 0, 0, 0),
        "A" => (0, 0, 0, 3, 0, 0, 0),
        "D" => (0, 0, 0, 0, 3, 6, 0),
        "FD" => (0, 0, 0, 3, 2, 2, 0),
        "TD" => (0, 0, 0, 2, 3, 2, 0),
        "RVM" => (1, 1, 3, 2, 0, 0, 0),
        "RVMI" => (1, 1, 3, 2, 0, 4, 0),
        "RVMR" => (1, 1, 3, 2, 0, 3, 0),
        "RMV" => (1, 2, 3, 1, 0, 0, 0),
        "VM" => (1, 2, 0, 3, 0, 0, 0),
        "VMI" => (1, 2, 0, 3, 1, 4, 0),
        "MVR" => (1, 3, 1, 2, 0, 0, 0),
        "MRV" => (1, 3, 2, 1, 0, 0, 0),
        _ => return Err(format!("unknown instruction encoding: {name}")),
    };
    flags.modrm = modrm;
    flags.modrm_idx = rm;
    flags.modreg_idx = reg;
    flags.vexreg_idx = vex;
    flags.imm_idx = imm;
    flags.imm_control = control;
    flags.zeroreg_val = zero;
    Ok(flags)
}

pub fn op_order(name: &str) -> Result<[&'static str; 4]> {
    let flags = encoding(name)?;
    let mut order = ["modrm", "modreg", "vexreg", "imm"];
    order.sort_by_key(|name| flags.idx(name) ^ 3);
    Ok(order)
}

#[derive(Clone, Debug)]
pub struct OpKind {
    pub regkind: char,
    pub sizestr: String,
}

impl OpKind {
    pub const SZ_OP: i32 = -1;
    pub const SZ_VEC: i32 = -2;
    pub const SZ_VEC_HALF: i32 = -3;
    pub const SZ_VEC_QUARTER: i32 = -4;
    pub const SZ_VEC_EIGHTH: i32 = -5;

    pub fn parse(op: &str) -> Result<Self> {
        let mut chars = op.chars();
        let regkind = chars.next().ok_or("empty operand kind")?;
        if !"IAJMOREBGPNQVHLUWSCDFKTZ".contains(regkind) {
            return Err(format!("unknown operand register kind: {regkind}"));
        }
        let sizestr = chars.as_str();
        Ok(Self {
            regkind,
            sizestr: sizestr.into(),
        })
    }

    pub fn kind(&self) -> &'static str {
        match self.regkind {
            'I' | 'A' | 'J' => "IMM",
            'M' | 'O' => "MEM",
            'R' | 'B' | 'E' | 'G' => "GP",
            'P' | 'N' | 'Q' => "MMX",
            'V' | 'H' | 'L' | 'U' | 'W' => "XMM",
            'S' => "SEG",
            'C' => "CR",
            'D' => "DR",
            'F' => "FPU",
            'K' => "MASK",
            'T' => "TMM",
            'Z' => "BND",
            other => panic!("unknown operand register kind: {other}"),
        }
    }

    fn parse_size(size: &str) -> Option<i32> {
        Some(match size {
            "b" => 1,
            "w" => 2,
            "d" | "ss" | "zd" => 4,
            "q" | "sd" | "zq" => 8,
            "t" => 10,
            "dq" => 16,
            "qq" => 32,
            "oq" => 64,
            "" => 0,
            "v" | "y" | "z" | "a" | "p" | "bs" => Self::SZ_OP,
            "x" | "pd" | "ps" => Self::SZ_VEC,
            "h" => Self::SZ_VEC_HALF,
            "f" => Self::SZ_VEC_QUARTER,
            "e" => Self::SZ_VEC_EIGHTH,
            _ => return None,
        })
    }

    pub fn size(&self) -> i32 {
        self.checked_size().expect("evaluated operand size")
    }

    fn checked_size(&self) -> Result<i32> {
        Self::parse_size(&self.sizestr)
            .ok_or_else(|| format!("unknown operand size: {}", self.sizestr))
    }

    pub fn abssize(&self, opsz: Option<i32>, vecsz: Option<i32>) -> Result<i32> {
        match self.checked_size()? {
            Self::SZ_OP => opsz,
            Self::SZ_VEC => vecsz,
            Self::SZ_VEC_HALF => vecsz.map(|size| size >> 1),
            Self::SZ_VEC_QUARTER => vecsz.map(|size| size >> 2),
            Self::SZ_VEC_EIGHTH => vecsz.map(|size| size >> 3),
            size => Some(size),
        }
        .ok_or_else(|| "unspecified operand size".into())
    }

    pub fn immsize(&self, opsz: i32) -> Result<i32> {
        let max_size = if self.sizestr == "bs" {
            1
        } else if self.sizestr.starts_with('z') {
            4
        } else {
            if self.sizestr.is_empty() {
                return Err("immediate operand has no size".into());
            }
            8
        };
        Ok(max_size.min(self.abssize(Some(opsz), None)?))
    }
}

impl PartialEq for OpKind {
    fn eq(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.size() == other.size()
    }
}

impl Eq for OpKind {}

impl Hash for OpKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind().hash(state);
        self.size().hash(state);
    }
}

impl PartialOrd for OpKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OpKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.kind(), self.size()).cmp(&(other.kind(), other.size()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstrDesc {
    pub mnemonic: String,
    pub encoding: String,
    pub operands: Vec<OpKind>,
    pub flags: BTreeSet<String>,
}

impl InstrDesc {
    pub fn parse(desc: &str) -> Result<Self> {
        let fields: Vec<_> = desc
            .split(is_whitespace)
            .filter(|field| !field.is_empty())
            .collect();
        if fields.len() < 6 {
            return Err(format!(
                "instruction description requires six fields: {desc}"
            ));
        }
        let (mnemonic, compact) = fields[5].split_once('+').unwrap_or((fields[5], ""));
        let mut flags: BTreeSet<String> = fields[6..].iter().map(|flag| (*flag).into()).collect();
        for flag in compact.chars() {
            flags.insert(
                match flag {
                    'w' => "INSTR_WIDTH",
                    'a' => "U67",
                    's' => "USEG",
                    'k' => "MASK",
                    'b' => "BCST",
                    'e' => "SAE",
                    'r' => "ER",
                    _ => return Err(format!("unknown compact instruction flag: {flag}")),
                }
                .into(),
            );
        }
        encoding(fields[0])?;
        let operands = fields[1..5]
            .iter()
            .filter(|op| **op != "-")
            .map(|op| OpKind::parse(op))
            .collect::<Result<_>>()?;
        Ok(Self {
            mnemonic: mnemonic.into(),
            encoding: fields[0].into(),
            operands,
            flags,
        })
    }

    pub fn imm_size(&self, opsz: i32) -> Result<i32> {
        let flags = encoding(&self.encoding)?;
        if flags.imm_control < 3 {
            return Ok(0);
        }
        if flags.imm_control == 3 {
            return Ok(1);
        }
        if self.mnemonic == "ENTER" {
            return Ok(3);
        }
        self.operand(flags.imm_idx ^ 3)?.immsize(opsz)
    }

    pub fn operand(&self, idx: u8) -> Result<&OpKind> {
        self.operands
            .get(usize::from(idx))
            .ok_or_else(|| format!("missing operand {idx} in {self:?}"))
    }

    pub fn dynsizes(&self) -> Result<BTreeSet<i32>> {
        let mut sizes = BTreeSet::new();
        for operand in &self.operands {
            let size = operand.checked_size()?;
            if size < 0 {
                sizes.insert(size);
            }
        }
        if self.has_any(&["INSTR_WIDTH", "SZ8"]) {
            sizes.insert(OpKind::SZ_OP);
        }
        if sizes.contains(&OpKind::SZ_OP) && sizes.len() > 1 {
            return Err(format!("conflicting dynamic operand sizes in {self:?}"));
        }
        Ok(sizes)
    }

    pub fn has_any(&self, flags: &[&str]) -> bool {
        flags.iter().any(|flag| self.flags.contains(*flag))
    }

    pub fn encode(&self, mnem: &str, ign66: bool, modrm: bool) -> Result<[u16; 3]> {
        let mut flags = encoding(&self.encoding)?;
        let dynamic = self.dynsizes()?;
        let mut dynsizes = Vec::with_capacity(2);
        if !dynamic.is_empty() && !dynamic.contains(&OpKind::SZ_OP) {
            if self.has_any(&["SZ8", "D64", "F64", "INSTR_WIDTH", "LOCK", "U66"]) {
                return Err(format!("incompatible flags in {self:?}"));
            }
            dynsizes.push(OpKind::SZ_VEC);
            dynsizes.extend(
                dynamic
                    .iter()
                    .copied()
                    .filter(|size| *size != OpKind::SZ_VEC),
            );
            flags.opsize = 4 | (OpKind::SZ_VEC - dynsizes[dynsizes.len() - 1]) as u8;
            if dynsizes.len() > 2 {
                return Err(format!("conflicting vector operand sizes in {self:?}"));
            }
        } else {
            if !self.flags.contains("SZ8") {
                dynsizes.push(OpKind::SZ_OP);
            }
            if self.flags.contains("D64") {
                flags.opsize = 2;
            }
            if self.flags.contains("F64") {
                flags.opsize = 3;
            }
            flags.lock = u8::from(self.flags.contains("LOCK"));
        }
        if self.has_any(&["SZ8", "INSTR_WIDTH"])
            || matches!(mnem, "MOVSX" | "MOVZX" | "XCHG_NOP" | "3DNOW")
        {
            flags.legacy = 1;
            if self.flags.contains("INSTR_WIDTH") && !self.flags.contains("SZ8") {
                flags.instr_width = 1;
            }
        }
        let imm_byte = self.imm_size(4)? == 1;
        flags.imm_control |= u8::from(imm_byte);
        let fixed = self
            .operands
            .iter()
            .filter(|op| op.size() >= 0 && !(imm_byte && op.kind() == "IMM"))
            .map(|op| size_code(op.size()))
            .collect::<Result<BTreeSet<_>>>()?;
        let mut fixed: Vec<_> = fixed.into_iter().collect();
        fixed.sort_by_key(|size| (1..=4).contains(size));
        if fixed.len() > 2 || (fixed.len() == 2 && !(1..=4).contains(&fixed[1])) {
            return Err(format!("invalid fixed sizes {fixed:?} in {self:?}"));
        }
        let mut sizes = fixed;
        sizes.resize(2, 1);
        sizes.extend(dynsizes.iter().copied());
        flags.size_fix1 = sizes[0] as u8;
        flags.size_fix2 = (sizes[1] - 1) as u8;
        let order = op_order(&self.encoding)?;
        for (i, operand) in self.operands.iter().enumerate() {
            let size = if operand.size() >= 0 {
                size_code(operand.size())?
            } else {
                operand.size()
            };
            if operand.kind() == "IMM" {
                if imm_byte && size != 1 && dynsizes.first() != Some(&size) {
                    return Err(format!("imm_byte with opsize {size} in {self:?}"));
                }
                flags.imm_size = if imm_byte {
                    u8::from(size == 1)
                } else {
                    size_index(&sizes, size)?
                };
            } else {
                let name = *order
                    .get(i)
                    .ok_or_else(|| format!("too many operands in {self:?}"))?;
                flags.set_operand(
                    name,
                    size_index(&sizes, size)?,
                    register_type(name, operand.kind())?,
                )?;
            }
        }
        if self.flags.contains("VSIB") {
            flags.vsib = 1;
        }
        if self.flags.contains("BCST") {
            flags.evex_bcst = 1;
        }
        if self.flags.contains("BCST16") {
            flags.evex_bcst16 = 1;
        }
        if self.flags.contains("MASK") {
            flags.evex_mask = 1;
        }
        if self.flags.contains("SAE") {
            flags.evex_rc = 1;
        }
        if self.flags.contains("ER") {
            flags.evex_rc = 3;
        }
        if modrm {
            flags.modrm = 1;
        }
        if !self.flags.contains("U66") && (ign66 || self.flags.contains("I66")) {
            flags.ign66 = 1;
        }
        let packed = flags.encode();
        Ok([packed as u16, (packed >> 16) as u16, (packed >> 32) as u16])
    }
}

fn size_code(size: i32) -> Result<i32> {
    match size {
        0 | 10 => Ok(0),
        1 => Ok(1),
        2 => Ok(2),
        4 => Ok(3),
        8 => Ok(4),
        16 => Ok(5),
        32 => Ok(6),
        64 => Ok(7),
        _ => Err(format!("invalid fixed operand size: {size}")),
    }
}

fn size_index(sizes: &[i32], size: i32) -> Result<u8> {
    sizes
        .iter()
        .position(|candidate| *candidate == size)
        .map(|index| index as u8)
        .ok_or_else(|| format!("operand size {size} is absent from {sizes:?}"))
}

fn register_type(name: &str, kind: &str) -> Result<u8> {
    match (name, kind) {
        ("modrm" | "modreg" | "vexreg", "GP") => Ok(1),
        ("modrm" | "modreg" | "vexreg", "XMM") => Ok(0),
        ("modrm" | "modreg", "MMX") => Ok(5),
        ("modrm", "FPU") => Ok(4),
        ("vexreg", "FPU" | "TMM") => Ok(3),
        ("modrm" | "modreg", "TMM") => Ok(6),
        ("modrm" | "modreg", "MASK") => Ok(7),
        ("vexreg", "MASK") => Ok(2),
        ("modreg", "SEG") => Ok(3),
        ("modreg", "DR" | "CR") => Ok(0),
        ("modrm", "MEM") => Ok(0),
        ("imm", "MEM" | "IMM" | "XMM") => Ok(0),
        _ => Err(format!("invalid register type {kind} for {name}")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Opcode {
    pub prefix: Option<String>,
    pub escape: u8,
    pub opc: u8,
    pub extended: bool,
    pub modrm: (Option<char>, Option<u8>, Option<u8>),
    pub vex: u8,
    pub vexl: Option<String>,
    pub rexw: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub line: usize,
    pub weak: bool,
    pub opcode: Opcode,
    pub desc: InstrDesc,
}

pub(crate) fn is_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}
