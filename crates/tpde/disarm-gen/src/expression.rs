// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;

const MAX_SOURCE_BYTES: usize = 65_536;
const MAX_TOKENS: usize = 4_096;
const MAX_DEPTH: usize = 256;
const MAX_PARSE_DEPTH: usize = MAX_DEPTH * 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CType {
    Bool,
    Signed(u8),
    Unsigned(u8),
    Float(u8),
    Register,
    Opaque,
}

impl CType {
    pub fn from_c(name: &str) -> Result<Self, String> {
        match name.trim() {
            "bool" | "_Bool" => Ok(Self::Bool),
            "signed char" | "int8_t" | "i8" => Ok(Self::Signed(8)),
            "unsigned char" | "uint8_t" | "u8" => Ok(Self::Unsigned(8)),
            "short" | "short int" | "signed short" | "int16_t" | "i16" => Ok(Self::Signed(16)),
            "unsigned short" | "unsigned short int" | "uint16_t" | "u16" => Ok(Self::Unsigned(16)),
            "int" | "signed" | "signed int" | "int32_t" | "i32" => Ok(Self::Signed(32)),
            "unsigned" | "unsigned int" | "uint32_t" | "u32" => Ok(Self::Unsigned(32)),
            "long"
            | "long int"
            | "long long"
            | "long long int"
            | "signed long"
            | "signed long int"
            | "signed long long"
            | "signed long long int"
            | "int64_t"
            | "intptr_t"
            | "ptrdiff_t"
            | "i64"
            | "isize" => Ok(Self::Signed(64)),
            "unsigned long"
            | "unsigned long int"
            | "unsigned long long"
            | "unsigned long long int"
            | "uint64_t"
            | "uintptr_t"
            | "size_t"
            | "u64"
            | "usize" => Ok(Self::Unsigned(64)),
            "float" | "f32" => Ok(Self::Float(32)),
            "double" | "f64" => Ok(Self::Float(64)),
            "DA_GReg" | "DA_GRegZR" | "DA_GRegSP" | "DA_VReg" => Ok(Self::Register),
            "Da64Cond" | "enum Da64Cond" | "Da64PrfOp" | "enum Da64PrfOp" => Ok(Self::Unsigned(32)),
            _ => Err(format!("unsupported C expression type {name:?}")),
        }
    }

    fn rust(self) -> Result<&'static str, String> {
        match self {
            Self::Bool => Ok("bool"),
            Self::Signed(8) => Ok("i8"),
            Self::Signed(16) => Ok("i16"),
            Self::Signed(32) => Ok("i32"),
            Self::Signed(64) => Ok("i64"),
            Self::Unsigned(8) => Ok("u8"),
            Self::Unsigned(16) => Ok("u16"),
            Self::Unsigned(32) => Ok("u32"),
            Self::Unsigned(64) => Ok("u64"),
            Self::Float(32) => Ok("f32"),
            Self::Float(64) => Ok("f64"),
            _ => Err(format!("type {self:?} cannot be cast to a Rust primitive")),
        }
    }

    fn promoted(self) -> Self {
        match self {
            Self::Bool | Self::Signed(8 | 16) | Self::Unsigned(8 | 16) => Self::Signed(32),
            _ => self,
        }
    }

    fn is_integer(self) -> bool {
        matches!(self, Self::Bool | Self::Signed(_) | Self::Unsigned(_))
    }
}

#[derive(Clone, Debug)]
pub struct Function {
    pub rust_name: String,
    pub arguments: Vec<CType>,
    pub result: CType,
}

#[derive(Clone, Debug, Default)]
pub struct Context {
    pub variables: BTreeMap<String, CType>,
    pub functions: BTreeMap<String, Function>,
    pub call_prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Expr {
    Integer {
        value: u64,
        ty: CType,
    },
    Identifier(String),
    Unary {
        op: String,
        operand: Box<Expr>,
    },
    Binary {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Conditional {
        condition: Box<Expr>,
        yes: Box<Expr>,
        no: Box<Expr>,
    },
    Cast {
        ty: CType,
        operand: Box<Expr>,
    },
    Call {
        name: String,
        arguments: Vec<Expr>,
    },
}

#[derive(Clone, Debug)]
pub struct Rendered {
    pub code: String,
    pub ty: CType,
}

impl Rendered {
    pub fn cast(&self, ty: CType) -> Result<String, String> {
        if ty == CType::Opaque && self.ty == CType::Opaque {
            return Ok(self.code.clone());
        }
        if ty == CType::Bool {
            return self.truth();
        }
        if matches!(self.ty, CType::Register | CType::Opaque) {
            return Err(format!(
                "cannot cast {:?} expression {} to {ty:?}",
                self.ty, self.code
            ));
        }
        if self.ty == CType::Bool && matches!(ty, CType::Float(_)) {
            return Ok(format!("(({} as u8) as {})", self.code, ty.rust()?));
        }
        Ok(format!("({} as {})", self.code, ty.rust()?))
    }

    pub fn truth(&self) -> Result<String, String> {
        match self.ty {
            CType::Bool => Ok(format!("({})", self.code)),
            CType::Signed(_) | CType::Unsigned(_) => Ok(format!("({} != 0)", self.code)),
            CType::Float(_) => Ok(format!("({} != 0.0)", self.code)),
            _ => Err(format!("non-scalar condition {}", self.code)),
        }
    }
}

#[derive(Clone, Debug)]
enum Token {
    Integer(u64, CType),
    Identifier(String),
    Operator(&'static str),
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
}

fn literal(text: &str) -> Result<(u64, CType), String> {
    let digits = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let suffix = text[digits.len()..].to_ascii_lowercase();
    if !matches!(
        suffix.as_str(),
        "" | "u" | "l" | "ul" | "lu" | "ll" | "ull" | "llu"
    ) {
        return Err(format!("invalid integer suffix in {text:?}"));
    }
    let (digits, radix, decimal) = if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (rest, 16, false)
    } else if let Some(rest) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        (rest, 2, false)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (&digits[1..], 8, false)
    } else {
        (digits, 10, true)
    };
    let value = u64::from_str_radix(digits, radix)
        .map_err(|_| format!("invalid or oversized integer literal {text:?}"))?;
    let unsigned = suffix.contains('u');
    let long = suffix.contains('l');
    let candidates: &[CType] = match (unsigned, long, decimal) {
        (true, true, _) => &[CType::Unsigned(64)],
        (true, false, _) => &[CType::Unsigned(32), CType::Unsigned(64)],
        (false, true, true) => &[CType::Signed(64)],
        (false, true, false) => &[CType::Signed(64), CType::Unsigned(64)],
        (false, false, true) => &[CType::Signed(32), CType::Signed(64)],
        (false, false, false) => &[
            CType::Signed(32),
            CType::Unsigned(32),
            CType::Signed(64),
            CType::Unsigned(64),
        ],
    };
    for &ty in candidates {
        let fits = match ty {
            CType::Unsigned(64) => true,
            CType::Unsigned(bits) => value < 1u64 << bits,
            CType::Signed(bits) => value < 1u64 << (bits - 1),
            _ => false,
        };
        if fits {
            return Ok((value, ty));
        }
    }
    Err(format!("integer literal {text:?} exceeds its C type"))
}

fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!("expression exceeds {MAX_SOURCE_BYTES} bytes"));
    }
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut position = 0;
    let mut parentheses = 0usize;
    while position < bytes.len() {
        if result.len() == MAX_TOKENS {
            return Err(format!("expression exceeds {MAX_TOKENS} tokens"));
        }
        let byte = bytes[position];
        if byte.is_ascii_whitespace() {
            position += 1;
            continue;
        }
        if byte == b'@' {
            position += 1;
            if !bytes
                .get(position)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            {
                return Err(format!(
                    "missing identifier after @ at byte {}",
                    position - 1
                ));
            }
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = position;
            position += 1;
            while bytes
                .get(position)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                position += 1;
            }
            result.push(Token::Identifier(source[start..position].to_owned()));
            continue;
        }
        if byte.is_ascii_digit() {
            let start = position;
            position += 1;
            while bytes.get(position).is_some_and(u8::is_ascii_alphanumeric) {
                position += 1;
            }
            let (value, ty) = literal(&source[start..position])?;
            result.push(Token::Integer(value, ty));
            continue;
        }
        let rest = &source[position..];
        let op = [
            "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+", "-", "*", "/", "%", "~", "!", "&",
            "|", "^", "<", ">", "(", ")", "?", ":", ",",
        ]
        .into_iter()
        .find(|op| rest.starts_with(op))
        .ok_or_else(|| format!("invalid expression token at byte {position} in {source:?}"))?;
        result.push(Token::Operator(op));
        if op == "(" {
            parentheses += 1;
            if parentheses > MAX_DEPTH {
                return Err(format!("expression exceeds nesting limit {MAX_DEPTH}"));
            }
        } else if op == ")" {
            parentheses = parentheses.saturating_sub(1);
        }
        position += op.len();
    }
    Ok(result)
}

impl Parser {
    fn consume(&mut self, op: &str) -> bool {
        if matches!(self.tokens.get(self.position), Some(Token::Operator(found)) if *found == op) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, op: &str) -> Result<(), String> {
        if self.consume(op) {
            Ok(())
        } else {
            Err(format!(
                "expected {op:?} at expression token {}",
                self.position
            ))
        }
    }

    fn cast_type(&mut self) -> Option<CType> {
        let start = self.position;
        let mut names = Vec::new();
        while let Some(Token::Identifier(name)) = self.tokens.get(self.position) {
            names.push(name.as_str());
            self.position += 1;
        }
        let ty = CType::from_c(&names.join(" ")).ok();
        if ty.is_some() && self.consume(")") {
            ty
        } else {
            self.position = start;
            None
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        if self.depth == MAX_PARSE_DEPTH {
            return Err("expression parser recursion budget exceeded".to_owned());
        }
        self.depth += 1;
        let result = self.primary_inner();
        self.depth -= 1;
        result
    }

    fn primary_inner(&mut self) -> Result<Expr, String> {
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or_else(|| "expected expression before end of input".to_owned())?;
        self.position += 1;
        match token {
            Token::Integer(value, ty) => Ok(Expr::Integer { value, ty }),
            Token::Identifier(name) => {
                if !self.consume("(") {
                    return Ok(Expr::Identifier(name));
                }
                let mut arguments = Vec::new();
                if !self.consume(")") {
                    loop {
                        arguments.push(self.expression(0)?);
                        if self.consume(")") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                Ok(Expr::Call { name, arguments })
            }
            Token::Operator(op @ ("+" | "-" | "!" | "~")) => Ok(Expr::Unary {
                op: op.to_owned(),
                operand: Box::new(self.primary()?),
            }),
            Token::Operator("(") => {
                if let Some(ty) = self.cast_type() {
                    return Ok(Expr::Cast {
                        ty,
                        operand: Box::new(self.primary()?),
                    });
                }
                let expression = self.expression(0)?;
                self.expect(")")?;
                Ok(expression)
            }
            _ => Err(format!(
                "unexpected token {token:?} at {}",
                self.position - 1
            )),
        }
    }

    fn expression(&mut self, minimum: u8) -> Result<Expr, String> {
        if self.depth == MAX_PARSE_DEPTH {
            return Err("expression parser recursion budget exceeded".to_owned());
        }
        self.depth += 1;
        let result = self.expression_inner(minimum);
        self.depth -= 1;
        result
    }

    fn expression_inner(&mut self, minimum: u8) -> Result<Expr, String> {
        let mut left = self.primary()?;
        loop {
            let Some(Token::Operator(op)) = self.tokens.get(self.position) else {
                break;
            };
            let op = *op;
            if op == "?" && minimum == 0 {
                self.position += 1;
                let yes = self.expression(0)?;
                self.expect(":")?;
                let no = self.expression(0)?;
                left = Expr::Conditional {
                    condition: Box::new(left),
                    yes: Box::new(yes),
                    no: Box::new(no),
                };
                continue;
            }
            let precedence = match op {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "!=" => 6,
                "<" | "<=" | ">" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                _ => break,
            };
            if precedence < minimum {
                break;
            }
            self.position += 1;
            let right = self.expression(precedence + 1)?;
            left = Expr::Binary {
                op: op.to_owned(),
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }
}

pub fn parse(source: &str) -> Result<Expr, String> {
    let mut parser = Parser {
        tokens: tokenize(source)?,
        position: 0,
        depth: 0,
    };
    let expression = parser.expression(0)?;
    if parser.position != parser.tokens.len() {
        return Err(format!(
            "trailing expression token {} in {source:?}",
            parser.position
        ));
    }
    let mut pending = vec![(&expression, 1usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(format!("expression exceeds AST depth {MAX_DEPTH}"));
        }
        match node {
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. } => {
                pending.push((operand, depth + 1))
            }
            Expr::Binary { left, right, .. } => {
                pending.push((left, depth + 1));
                pending.push((right, depth + 1));
            }
            Expr::Conditional { condition, yes, no } => {
                pending.push((condition, depth + 1));
                pending.push((yes, depth + 1));
                pending.push((no, depth + 1));
            }
            Expr::Call { arguments, .. } => {
                pending.extend(arguments.iter().map(|arg| (arg, depth + 1)))
            }
            _ => {}
        }
    }
    Ok(expression)
}

fn common_type(left: CType, right: CType) -> Result<CType, String> {
    let left = left.promoted();
    let right = right.promoted();
    match (left, right) {
        (CType::Float(a), CType::Float(b)) => Ok(CType::Float(a.max(b))),
        (CType::Float(bits), ty) | (ty, CType::Float(bits)) if ty.is_integer() => {
            Ok(CType::Float(bits))
        }
        (CType::Signed(a), CType::Signed(b)) => Ok(CType::Signed(a.max(b))),
        (CType::Unsigned(a), CType::Unsigned(b)) => Ok(CType::Unsigned(a.max(b))),
        (CType::Signed(signed), CType::Unsigned(unsigned))
        | (CType::Unsigned(unsigned), CType::Signed(signed)) => Ok(if signed > unsigned {
            CType::Signed(signed)
        } else {
            CType::Unsigned(unsigned)
        }),
        (CType::Opaque, CType::Opaque) => Ok(CType::Opaque),
        _ => Err(format!(
            "incompatible expression types {left:?} and {right:?}"
        )),
    }
}

pub fn identifier(name: &str) -> String {
    match name {
        "crate" | "self" | "Self" | "super" => format!("__disarm_ident_{name}"),
        "as" | "async" | "await" | "break" | "const" | "continue" | "dyn" | "else" | "enum"
        | "extern" | "false" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match"
        | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "static" | "struct" | "trait"
        | "true" | "type" | "unsafe" | "use" | "where" | "while" | "abstract" | "become"
        | "box" | "do" | "final" | "gen" | "macro" | "override" | "priv" | "try" | "typeof"
        | "unsized" | "virtual" | "yield" => {
            format!("r#{name}")
        }
        _ => name.to_owned(),
    }
}

fn builtin(name: &str) -> Option<Function> {
    let (arguments, result): (&[CType], CType) = match name {
        "da_sext" => (&[CType::Signed(64), CType::Unsigned(32)], CType::Signed(64)),
        "da_immadd" => (&[CType::Signed(64)], CType::Unsigned(32)),
        "da_immlogical" => (
            &[CType::Unsigned(64), CType::Unsigned(32)],
            CType::Unsigned(32),
        ),
        "da_immfmov32" => (&[CType::Float(32)], CType::Unsigned(32)),
        "da_immfmov64" => (&[CType::Float(64)], CType::Unsigned(32)),
        "da_immsimdmovi" => (&[CType::Unsigned(64)], CType::Unsigned(32)),
        _ => return None,
    };
    Some(Function {
        rust_name: name.to_owned(),
        arguments: arguments.to_vec(),
        result,
    })
}

impl Expr {
    pub fn parse(source: &str) -> Result<Self, String> {
        parse(source)
    }

    pub fn render(&self, context: &Context) -> Result<Rendered, String> {
        let mut remaining = MAX_TOKENS;
        self.render_bounded(context, 0, &mut remaining)
    }

    fn render_bounded(
        &self,
        context: &Context,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<Rendered, String> {
        if depth == MAX_DEPTH || *remaining == 0 {
            return Err("expression rendering budget exceeded".to_owned());
        }
        *remaining -= 1;
        let (code, ty) = match self {
            Self::Integer { value, ty } => (format!("{value}{}", ty.rust()?), *ty),
            Self::Identifier(name) if name == "true" || name == "false" => {
                (name.clone(), CType::Bool)
            }
            Self::Identifier(name) => (
                identifier(name),
                context
                    .variables
                    .get(name)
                    .copied()
                    .or_else(|| {
                        if name.starts_with("DA64I_") || name.starts_with("DA64G_") {
                            Some(CType::Signed(32))
                        } else if name.starts_with("DA64_HAVE_") {
                            Some(CType::Bool)
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        format!("C expression identifier has no declared type: {name}")
                    })?,
            ),
            Self::Unary { op, operand } => {
                let value = operand.render_bounded(context, depth + 1, remaining)?;
                let ty = value.ty.promoted();
                match op.as_str() {
                    "!" => (format!("((!{}) as i32)", value.truth()?), CType::Signed(32)),
                    "+" => (value.cast(ty)?, ty),
                    "-" if ty.is_integer() => (format!("({}).wrapping_neg()", value.cast(ty)?), ty),
                    "-" if matches!(ty, CType::Float(_)) => (format!("(-{})", value.cast(ty)?), ty),
                    "~" if ty.is_integer() => (format!("(!{})", value.cast(ty)?), ty),
                    _ => return Err(format!("invalid unary operator {op:?} for {ty:?}")),
                }
            }
            Self::Cast { ty, operand } => (
                operand
                    .render_bounded(context, depth + 1, remaining)?
                    .cast(*ty)?,
                *ty,
            ),
            Self::Binary { op, left, right } => {
                let left = left.render_bounded(context, depth + 1, remaining)?;
                let right = right.render_bounded(context, depth + 1, remaining)?;
                if op == "&&" || op == "||" {
                    (
                        format!("(({} {op} {}) as i32)", left.truth()?, right.truth()?),
                        CType::Signed(32),
                    )
                } else if op == "<<" || op == ">>" {
                    let ty = left.ty.promoted();
                    if !ty.is_integer() || !right.ty.is_integer() {
                        return Err("shift operands must be integers".to_owned());
                    }
                    let method = if op == "<<" {
                        "wrapping_shl"
                    } else {
                        "wrapping_shr"
                    };
                    (
                        format!(
                            "({}).{method}({})",
                            left.cast(ty)?,
                            right.cast(CType::Unsigned(32))?
                        ),
                        ty,
                    )
                } else {
                    let ty = common_type(left.ty, right.ty)?;
                    let lhs = left.cast(ty)?;
                    let rhs = right.cast(ty)?;
                    match op.as_str() {
                        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                            (format!("(({lhs} {op} {rhs}) as i32)"), CType::Signed(32))
                        }
                        "+" | "-" | "*" | "/" | "%" if ty.is_integer() => {
                            let method = match op.as_str() {
                                "+" => "wrapping_add",
                                "-" => "wrapping_sub",
                                "*" => "wrapping_mul",
                                "/" => "wrapping_div",
                                "%" => "wrapping_rem",
                                _ => unreachable!(),
                            };
                            (format!("({lhs}).{method}({rhs})"), ty)
                        }
                        "&" | "|" | "^" if ty.is_integer() => (format!("({lhs} {op} {rhs})"), ty),
                        "+" | "-" | "*" | "/" if matches!(ty, CType::Float(_)) => {
                            (format!("({lhs} {op} {rhs})"), ty)
                        }
                        _ => return Err(format!("invalid binary operator {op:?} for {ty:?}")),
                    }
                }
            }
            Self::Conditional { condition, yes, no } => {
                let condition = condition
                    .render_bounded(context, depth + 1, remaining)?
                    .truth()?;
                let yes = yes.render_bounded(context, depth + 1, remaining)?;
                let no = no.render_bounded(context, depth + 1, remaining)?;
                let ty = common_type(yes.ty, no.ty)?;
                (
                    format!(
                        "(if {condition} {{ {} }} else {{ {} }})",
                        yes.cast(ty)?,
                        no.cast(ty)?
                    ),
                    ty,
                )
            }
            Self::Call { name, arguments } if name == "DA_REGVAL" => {
                if arguments.len() != 1 {
                    return Err("DA_REGVAL requires exactly one argument".to_owned());
                }
                let value = arguments[0].render_bounded(context, depth + 1, remaining)?;
                (
                    format!("(({}).value() as u8)", value.code),
                    CType::Unsigned(8),
                )
            }
            Self::Call { name, arguments } => {
                let function = context
                    .functions
                    .get(name)
                    .cloned()
                    .or_else(|| builtin(name))
                    .ok_or_else(|| {
                        format!("C expression function has no declared signature: {name}")
                    })?;
                if function.arguments.len() != arguments.len() {
                    return Err(format!(
                        "{name} expects {} arguments, found {}",
                        function.arguments.len(),
                        arguments.len()
                    ));
                }
                let args = arguments
                    .iter()
                    .zip(&function.arguments)
                    .map(|(arg, &ty)| {
                        let value = arg.render_bounded(context, depth + 1, remaining)?;
                        value.cast(ty)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                (
                    format!("{}({})", function.rust_name, args.join(", ")),
                    function.result,
                )
            }
        };
        Ok(Rendered { code, ty })
    }
}

pub fn render(source: &str, context: &Context) -> Result<Rendered, String> {
    parse(source)?.render(context)
}

pub fn integer_with_context(source: &str, context: &Context) -> Result<String, String> {
    render(source, context)?.cast(CType::Unsigned(64))
}

pub fn integer(source: &str) -> Result<String, String> {
    integer_with_context(source, &Context::default())
}
