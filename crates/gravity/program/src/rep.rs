//! A lane's representation and lifetime, decided once by decompose (lowerv3 design §1.1); the runtime Lane block is unchanged.

/// A `numeric(p, s)` typmod's facts: `precision` 0 = none (an unconstrained numeric or not a numeric).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NumTypmod {
    pub precision: u16,
    pub scale: i16,
}

pub const NUM_WORD_DIGITS: u16 = 18;

impl NumTypmod {
    pub const NONE: NumTypmod = NumTypmod { precision: 0, scale: 0 };

    pub const fn of(precision: u16, scale: i16) -> NumTypmod {
        NumTypmod { precision, scale }
    }

    /// PostgreSQL's `numeric` typmod: `((p << 16) | (s & 0x7ff)) + 4`, the scale an 11-bit signed field; `NONE` below 4.
    pub const fn of_typmod(typmod: i32) -> NumTypmod {
        if typmod < 4 {
            return NumTypmod::NONE;
        }
        let t = typmod - 4;
        let s = ((t & 0x7ff) ^ 1024) - 1024;
        NumTypmod { precision: ((t >> 16) & 0xffff) as u16, scale: s as i16 }
    }

    pub const fn is_some(self) -> bool {
        self.precision != 0
    }

    pub const fn scale(self) -> Option<i16> {
        if self.is_some() { Some(self.scale) } else { None }
    }

    /// The mantissa fits one word at this scale (D197): p ≤ 18 and 0 ≤ s ≤ 18.
    pub const fn narrow(self) -> bool {
        self.is_some() && self.precision <= NUM_WORD_DIGITS && self.scale >= 0 && self.scale as u16 <= NUM_WORD_DIGITS
    }
}

/// How a lane's bytes are STORED (a closed set); what they MEAN is the type's type-facts row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rep {
    /// A by-value datum word of `bytes` (1/2/4/8): ints, dates, timestamps, oids, float4/8, bool (D198).
    Word { bytes: u8 },
    VarlenaPtr,
    /// A pointer to a fixed-length by-reference image (name, uuid, interval, timetz): never a varlena.
    FixedPtr { len: u16 },
    CstrPtr,
    Code { dict: DictRef },
    /// A packed numeric(p, s): `Window` = the pgrc lane (mantissa or verbatim per window), `Word` = the order word.
    PackedNum { facts: NumTypmod, form: NumForm },
    /// N14: a pgrc lane of an unconstrained numeric whose only readers are keyed min / max calls or (D3139) float casts
    /// (decompose `packed_minmax_only`): per window the chunk's PACKED_NUMERIC mantissas at the scale the lane's `ncodes_or_scale`
    /// carries (the chunk's elected scale, `KIND_FIXED`), or a verbatim chunk's images (`KIND_BYREF`).
    PackedNumDyn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DictRef {
    pub lane: u8,
    /// The `TableDecl::fingerprint(lane)` slot; `DictRef::NO_TABLE` until one is declared.
    pub fp_table: u8,
}

impl DictRef {
    pub const NO_TABLE: u8 = u8::MAX;

    pub const fn lane(lane: u8) -> DictRef {
        DictRef { lane, fp_table: DictRef::NO_TABLE }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumForm {
    Window,
    Word,
}

/// How long the bytes behind a word stay valid (ordered: a retain copies iff have < need).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Until {
    Call,
    Row,
    Window,
    Part,
    Life,
    Statement,
}

/// Where the bytes behind a word live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Home {
    Word,
    PagePin,
    PgrcWindow,
    Dictionary,
    Breaker,
    Immediate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaneLife {
    pub until: Until,
    pub home: Home,
}

impl LaneLife {
    pub const VALUE: LaneLife = LaneLife { until: Until::Statement, home: Home::Word };
    pub const PAGE: LaneLife = LaneLife { until: Until::Window, home: Home::PagePin };
    pub const PGRC: LaneLife = LaneLife { until: Until::Window, home: Home::PgrcWindow };
    pub const DICTIONARY: LaneLife = LaneLife { until: Until::Part, home: Home::Dictionary };
    pub const BREAKER: LaneLife = LaneLife { until: Until::Life, home: Home::Breaker };
    pub const IMMEDIATE: LaneLife = LaneLife { until: Until::Statement, home: Home::Immediate };
}

impl Rep {
    /// The storage of a SQL type's datum on its own (`typlen`/`typbyval`): a projected cell's `Rep` before any source says more.
    pub const fn of_storage(typlen: i16, byval: bool) -> Rep {
        match (byval, typlen) {
            (true, n) => Rep::Word { bytes: if n > 0 && n <= 8 { n as u8 } else { 8 } },
            (false, -1) => Rep::VarlenaPtr,
            (false, -2) => Rep::CstrPtr,
            (false, n) => Rep::FixedPtr { len: if n > 0 { n as u16 } else { 0 } },
        }
    }

    /// The dump's short name: `w8`, `vl`, `fx16`, `cs`, `code`, `pn<s>[w]`, `pnd`.
    pub fn short(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match *self {
            Rep::Word { bytes } => write!(out, "w{bytes}"),
            Rep::VarlenaPtr => write!(out, "vl"),
            Rep::FixedPtr { len } => write!(out, "fx{len}"),
            Rep::CstrPtr => write!(out, "cs"),
            Rep::Code { dict } if dict.fp_table == DictRef::NO_TABLE => write!(out, "code"),
            Rep::Code { dict } => write!(out, "code@t{}", dict.fp_table),
            Rep::PackedNum { facts, form } => {
                write!(out, "pn{}.{}", facts.precision, facts.scale)?;
                if !facts.narrow() {
                    write!(out, "w")?;
                }
                if form == NumForm::Word { write!(out, "/word") } else { Ok(()) }
            }
            Rep::PackedNumDyn => write!(out, "pnd"),
        }
    }
}

impl LaneLife {
    pub fn short(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        if *self == LaneLife::VALUE {
            return write!(out, "val");
        }
        let u = match self.until {
            Until::Call => "call",
            Until::Row => "row",
            Until::Window => "win",
            Until::Part => "part",
            Until::Life => "life",
            Until::Statement => "stmt",
        };
        let h = match self.home {
            Home::Word => "word",
            Home::PagePin => "page",
            Home::PgrcWindow => "arena",
            Home::Dictionary => "dict",
            Home::Breaker => "breaker",
            Home::Immediate => "imm",
        };
        write!(out, "{u}/{h}")
    }
}

/// What a function call's result lives as (the fmgr registry's answer, lowerv3 design §3): `Call` = the builtin's scratch,
/// overwritten by its next call on the thread from any site; `Row` = the row context or the caller's region; `Arg(k)` = argument k's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FnLife {
    Value,
    Call,
    Row,
    Arg(u8),
}

/// How long a computed value's bytes stay valid: a lane's, a constant's or a record's life; the next row; or until the next
/// call of builtin `proc` (0 = any builtin) on the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExprLife {
    Lane(LaneLife),
    Row,
    Call { proc: u32 },
}

impl ExprLife {
    const fn rank(self) -> u8 {
        match self {
            ExprLife::Call { .. } => 0,
            ExprLife::Row => 1,
            ExprLife::Lane(l) => 2 + l.until as u8,
        }
    }

    /// The shorter of two lives (a CASE / COALESCE result is any of its branches); two different scratches are any call's.
    pub fn min(self, other: ExprLife) -> ExprLife {
        match (self, other) {
            (ExprLife::Call { proc: a }, ExprLife::Call { proc: b }) if a != b => ExprLife::Call { proc: 0 },
            _ if other.rank() < self.rank() => other,
            _ => self,
        }
    }

    pub fn short(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            ExprLife::Lane(l) => l.short(out),
            ExprLife::Row => write!(out, "row"),
            ExprLife::Call { proc } => write!(out, "call{proc}"),
        }
    }
}

/// A computed expression's output: its storage, SQL type and life (decompose `outs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprOut {
    pub rep: Rep,
    pub typid: u32,
    pub life: ExprLife,
}
