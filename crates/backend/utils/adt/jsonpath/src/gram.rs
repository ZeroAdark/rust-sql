//! jsonpath_gram.y: Bison 2.3's LALR(1) tables driven by the yacc.c
//! skeleton's loop, with the grammar actions mirrored 1:1. The state and value
//! stacks live in the heap (no native recursion) and overflow exactly as
//! Bison's do at YYMAXDEPTH. Nodes are leaked into the caller's mcx (C's
//! palloc model, bulk-freed at context reset), so the tree is drop-free.

use core::cell::Cell;

use ::mcx::{alloc_in, leak_in, slice_in, Mcx, PgVec};
use ::types_core::DEFAULT_COLLATION_OID;
use ::types_error::{ereturn, PgError, PgResult, SoftErrorContext};
use ::types_error::{
    ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_INVALID_REGULAR_EXPRESSION, ERRCODE_SYNTAX_ERROR,
};

use crate::path::ItemType;
use crate::gram_tables::{
    YYCHECK, YYDEFACT, YYDEFGOTO, YYPACT, YYPGOTO, YYR1, YYR2, YYTABLE, YYTRANSLATE,
};
use crate::scan::{jsonpath_yyerror, jsonpath_yyerror_yytext, Lexer, Token};

pub struct ParseItem<'mcx> {
    pub typ: ItemType,
    pub next: Cell<Option<&'mcx ParseItem<'mcx>>>,
    pub value: ParseValue<'mcx>,
}

#[derive(Clone, Copy)]
pub enum ParseValue<'mcx> {
    None,
    Args {
        left: Option<&'mcx ParseItem<'mcx>>,
        right: Option<&'mcx ParseItem<'mcx>>,
    },
    Arg(Option<&'mcx ParseItem<'mcx>>),
    Array(&'mcx [Subscript<'mcx>]),
    AnyBounds {
        first: u32,
        last: u32,
    },
    LikeRegex {
        expr: Option<&'mcx ParseItem<'mcx>>,
        pattern: &'mcx [u8],
        flags: u32,
    },
    /// Full on-disk numeric varlena bytes (header included).
    Numeric(&'mcx [u8]),
    Boolean(bool),
    String(&'mcx [u8]),
}

#[derive(Clone, Copy)]
pub struct Subscript<'mcx> {
    pub from: Option<&'mcx ParseItem<'mcx>>,
    pub to: Option<&'mcx ParseItem<'mcx>>,
}

pub struct ParseResult<'mcx> {
    pub expr: &'mcx ParseItem<'mcx>,
    pub lax: bool,
}

pub const JSP_REGEX_ICASE: u32 = 0x01;
pub const JSP_REGEX_DOTALL: u32 = 0x02;
pub const JSP_REGEX_MLINE: u32 = 0x04;
pub const JSP_REGEX_WSPACE: u32 = 0x08;
pub const JSP_REGEX_QUOTE: u32 = 0x10;

type Item<'mcx> = &'mcx ParseItem<'mcx>;
type POut<T> = PgResult<Option<T>>;

fn make_item<'mcx>(
    mcx: Mcx<'mcx>,
    typ: ItemType,
    value: ParseValue<'mcx>,
) -> PgResult<Item<'mcx>> {
    crate::check_for_interrupts()?;
    Ok(leak_in(alloc_in(
        mcx,
        ParseItem { typ, next: Cell::new(None), value },
    )?))
}

fn make_item_type<'mcx>(mcx: Mcx<'mcx>, typ: ItemType) -> PgResult<Item<'mcx>> {
    make_item(mcx, typ, ParseValue::None)
}

fn make_item_string<'mcx>(mcx: Mcx<'mcx>, s: Option<&'mcx [u8]>) -> PgResult<Item<'mcx>> {
    match s {
        None => make_item_type(mcx, ItemType::Null),
        Some(s) => make_item(mcx, ItemType::String, ParseValue::String(s)),
    }
}

fn make_item_variable<'mcx>(mcx: Mcx<'mcx>, s: &'mcx [u8]) -> PgResult<Item<'mcx>> {
    make_item(mcx, ItemType::Variable, ParseValue::String(s))
}

fn make_item_key<'mcx>(mcx: Mcx<'mcx>, s: &'mcx [u8]) -> PgResult<Item<'mcx>> {
    make_item(mcx, ItemType::Key, ParseValue::String(s))
}

/// C: numeric_in(s->val, InvalidOid, -1) — hard error, matching the grammar
/// action's DirectFunctionCall3.
fn make_item_numeric<'mcx>(mcx: Mcx<'mcx>, s: &[u8]) -> PgResult<Item<'mcx>> {
    let text = core::str::from_utf8(s).expect("scanner numerics are ASCII");
    let img = adt_numeric::numeric_in(text, -1, None)?
        .expect("hard numeric_in returns Err, not soft None");
    let bytes = slice_in(mcx, img.as_bytes())?.leak();
    make_item(mcx, ItemType::Numeric, ParseValue::Numeric(bytes))
}

fn make_item_bool<'mcx>(mcx: Mcx<'mcx>, val: bool) -> PgResult<Item<'mcx>> {
    make_item(mcx, ItemType::Bool, ParseValue::Boolean(val))
}

fn make_item_binary<'mcx>(
    mcx: Mcx<'mcx>,
    typ: ItemType,
    la: Option<Item<'mcx>>,
    ra: Option<Item<'mcx>>,
) -> PgResult<Item<'mcx>> {
    make_item(mcx, typ, ParseValue::Args { left: la, right: ra })
}

/// C: makeItemUnary — folds +/- over a lone numeric literal.
fn make_item_unary<'mcx>(mcx: Mcx<'mcx>, typ: ItemType, a: Item<'mcx>) -> PgResult<Item<'mcx>> {
    if typ == ItemType::Plus && a.typ == ItemType::Numeric && a.next.get().is_none() {
        return Ok(a);
    }
    if typ == ItemType::Minus && a.typ == ItemType::Numeric && a.next.get().is_none() {
        let num = match a.value {
            ParseValue::Numeric(n) => n,
            _ => unreachable!("Numeric item without Numeric value"),
        };
        let negated = adt_numeric::numeric_uminus(adt_numeric::Num::from_payload(&num[4..]));
        let bytes = slice_in(mcx, negated.as_bytes())?.leak();
        return make_item(mcx, ItemType::Numeric, ParseValue::Numeric(bytes));
    }
    make_item(mcx, typ, ParseValue::Arg(Some(a)))
}

fn make_item_unary_optional<'mcx>(
    mcx: Mcx<'mcx>,
    typ: ItemType,
    arg: Option<Item<'mcx>>,
) -> PgResult<Item<'mcx>> {
    make_item(mcx, typ, ParseValue::Arg(arg))
}

/// C: makeItemList — chain the accessor list through ->next.
fn make_item_list<'mcx>(list: &[Item<'mcx>]) -> Item<'mcx> {
    debug_assert!(!list.is_empty());
    let head = list[0];
    let mut end = head;
    while let Some(n) = end.next.get() {
        end = n;
    }
    for &c in &list[1..] {
        end.next.set(Some(c));
        end = c;
    }
    head
}

fn make_index_array<'mcx>(mcx: Mcx<'mcx>, list: PgVec<'mcx, Item<'mcx>>) -> PgResult<Item<'mcx>> {
    debug_assert!(!list.is_empty());
    let mut elems: PgVec<'mcx, Subscript<'mcx>> = ::mcx::vec_with_capacity_in(mcx, list.len())?;
    for jpi in list.iter() {
        debug_assert_eq!(jpi.typ, ItemType::Subscript);
        let (from, to) = match jpi.value {
            ParseValue::Args { left, right } => (left, right),
            _ => unreachable!("Subscript item without Args value"),
        };
        elems.push(Subscript { from, to });
    }
    make_item(mcx, ItemType::IndexArray, ParseValue::Array(elems.leak()))
}

fn make_any<'mcx>(mcx: Mcx<'mcx>, first: i32, last: i32) -> PgResult<Item<'mcx>> {
    let f = if first >= 0 { first as u32 } else { u32::MAX };
    let l = if last >= 0 { last as u32 } else { u32::MAX };
    make_item(mcx, ItemType::Any, ParseValue::AnyBounds { first: f, last: l })
}

/// One server-encoding character starting the unrecognized flag text
/// (C: errdetail with pg_mblen bytes of the offending flag character).
fn first_char_lossy(rest: &[u8]) -> String {
    match core::str::from_utf8(rest) {
        Ok(s) => s.chars().next().map(String::from).unwrap_or_default(),
        Err(e) if e.valid_up_to() > 0 => {
            let s = core::str::from_utf8(&rest[..e.valid_up_to()]).unwrap();
            s.chars().next().map(String::from).unwrap_or_default()
        }
        Err(_) => String::from_utf8_lossy(&rest[..1]).into_owned(),
    }
}

/// C: makeItemLikeRegex. Ok(None) = soft error recorded (grammar YYABORT).
fn make_item_like_regex<'mcx>(
    mcx: Mcx<'mcx>,
    expr: Option<Item<'mcx>>,
    pattern: &'mcx [u8],
    flags: Option<&[u8]>,
    escontext: &mut Option<&mut SoftErrorContext>,
) -> POut<Item<'mcx>> {
    let mut xflags: u32 = 0;
    if let Some(fbytes) = flags {
        for (i, &c) in fbytes.iter().enumerate() {
            match c {
                b'i' => xflags |= JSP_REGEX_ICASE,
                b's' => xflags |= JSP_REGEX_DOTALL,
                b'm' => xflags |= JSP_REGEX_MLINE,
                b'x' => xflags |= JSP_REGEX_WSPACE,
                b'q' => xflags |= JSP_REGEX_QUOTE,
                _ => {
                    return ereturn(
                        escontext.as_deref_mut(),
                        None,
                        PgError::error("invalid input syntax for type jsonpath")
                            .with_sqlstate(ERRCODE_SYNTAX_ERROR)
                            .with_detail(format!(
                                "Unrecognized flag character \"{}\" in LIKE_REGEX predicate.",
                                first_char_lossy(&fbytes[i..])
                            )),
                    );
                }
            }
        }
    }

    let cflags = match jsp_convert_regex_flags(xflags, escontext.as_deref_mut())? {
        Some(c) => c,
        None => return Ok(None),
    };

    // C: validity check only — pg_regcomp + pg_regfree over the wide pattern.
    let wpattern = mbutils::pg_mb2wchar_with_len(mcx, pattern)?;
    // regcomp.c:403: pg_regcomp compiles under its collation's ctype.
    regex_core::regex_locale::pg_set_regex_collation(mcx, DEFAULT_COLLATION_OID)?;
    if let Err(e) =
        regex_core::regex_compile::pg_regcomp(mcx, &wpattern, cflags, DEFAULT_COLLATION_OID)
    {
        // REG_CANCEL is the engine's consumed-interrupt sentinel: the cancel
        // or termination must propagate, never be downgraded to a syntax
        // error (the regex seam's interception rule).
        if e.0 == regex_core::regex_consts::REG_CANCEL {
            return Err(regex_core::regex_error::take_cancel_error());
        }
        let msg = regex_core::regex_export_free_error::pg_regerror(e.0);
        return ereturn(
            escontext.as_deref_mut(),
            None,
            PgError::error(format!("invalid regular expression: {msg}"))
                .with_sqlstate(ERRCODE_INVALID_REGULAR_EXPRESSION),
        );
    }

    Ok(Some(make_item(
        mcx,
        ItemType::LikeRegex,
        ParseValue::LikeRegex { expr, pattern, flags: xflags },
    )?))
}

/// C: jspConvertRegexFlags (jsonpath_gram.y) — XQuery flag bits to REG_* cflags.
pub fn jsp_convert_regex_flags(
    xflags: u32,
    escontext: Option<&mut SoftErrorContext>,
) -> POut<i32> {
    use regex_core::regex_consts::{REG_ADVANCED, REG_ICASE, REG_NLANCH, REG_NLSTOP, REG_QUOTE};

    let mut cflags: i32 = REG_ADVANCED;
    if xflags & JSP_REGEX_ICASE != 0 {
        cflags |= REG_ICASE;
    }
    // Per XQuery spec, 'q' makes 'm', 's', 'x' ignored.
    if xflags & JSP_REGEX_QUOTE != 0 {
        cflags &= !REG_ADVANCED;
        cflags |= REG_QUOTE;
    } else {
        if xflags & JSP_REGEX_DOTALL == 0 {
            cflags |= REG_NLSTOP;
        }
        if xflags & JSP_REGEX_MLINE != 0 {
            cflags |= REG_NLANCH;
        }
        if xflags & JSP_REGEX_WSPACE != 0 {
            return ereturn(
                escontext,
                None,
                PgError::error(
                    "XQuery \"x\" flag (expanded regular expressions) is not implemented",
                )
                .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
            );
        }
    }
    Ok(Some(cflags))
}

const YYFINAL: i32 = 5;
const YYLAST: i32 = 239;
const YYNTOKENS: i32 = 68;
const YYPACT_NINF: i16 = -47;
const YYTABLE_NINF: i16 = -137;
const YYINITDEPTH: usize = 200;
const YYMAXDEPTH: usize = 10000;
const YYEOF: i32 = 0;

fn yytranslate(tok: Token) -> i32 {
    let code: usize = match tok {
        Token::Char(c) => c as usize,
        Token::ToP => 258,
        Token::NullP => 259,
        Token::TrueP => 260,
        Token::FalseP => 261,
        Token::IsP => 262,
        Token::UnknownP => 263,
        Token::ExistsP => 264,
        Token::IdentP => 265,
        Token::StringP => 266,
        Token::NumericP => 267,
        Token::IntP => 268,
        Token::VariableP => 269,
        Token::OrP => 270,
        Token::AndP => 271,
        Token::NotP => 272,
        Token::LessP => 273,
        Token::LessEqualP => 274,
        Token::EqualP => 275,
        Token::NotEqualP => 276,
        Token::GreaterEqualP => 277,
        Token::GreaterP => 278,
        Token::AnyP => 279,
        Token::StrictP => 280,
        Token::LaxP => 281,
        Token::LastP => 282,
        Token::StartsP => 283,
        Token::WithP => 284,
        Token::LikeRegexP => 285,
        Token::FlagP => 286,
        Token::AbsP => 287,
        Token::SizeP => 288,
        Token::TypeP => 289,
        Token::FloorP => 290,
        Token::DoubleP => 291,
        Token::CeilingP => 292,
        Token::KeyValueP => 293,
        Token::DatetimeP => 294,
        Token::BigintP => 295,
        Token::BooleanP => 296,
        Token::DateP => 297,
        Token::DecimalP => 298,
        Token::IntegerP => 299,
        Token::NumberP => 300,
        Token::StringFuncP => 301,
        Token::TimeP => 302,
        Token::TimeTzP => 303,
        Token::TimestampP => 304,
        Token::TimestampTzP => 305,
    };
    YYTRANSLATE[code] as i32
}

/// C's List of parse items (list_make1 / lappend), arena cells.
struct ListCell<'mcx> {
    item: Item<'mcx>,
    next: Cell<Option<&'mcx ListCell<'mcx>>>,
}

#[derive(Clone, Copy)]
struct List<'mcx> {
    head: &'mcx ListCell<'mcx>,
    tail: &'mcx ListCell<'mcx>,
}

impl<'mcx> List<'mcx> {
    fn items_in(self, mcx: Mcx<'mcx>) -> PgResult<PgVec<'mcx, Item<'mcx>>> {
        let mut v = PgVec::new_in(mcx);
        let mut c = Some(self.head);
        while let Some(cell) = c {
            ::mcx::pgvec_reserve(&mut v, 1)?;
            v.push(cell.item);
            c = cell.next.get();
        }
        Ok(v)
    }
}

/// YYSTYPE: the %union members the grammar's symbols carry.
#[derive(Clone, Copy)]
enum Value<'mcx> {
    Empty,
    Str(&'mcx [u8]),
    Item(Item<'mcx>),
    OptItem(Option<Item<'mcx>>),
    Bool(bool),
    Op(ItemType),
    Int(i32),
    List(Option<List<'mcx>>),
}

fn take<'mcx>(v: &mut Value<'mcx>) -> Value<'mcx> {
    core::mem::replace(v, Value::Empty)
}

fn v_str<'mcx>(v: &mut Value<'mcx>) -> &'mcx [u8] {
    match take(v) {
        Value::Str(s) => s,
        _ => unreachable!("grammar symbol is not a token string"),
    }
}

fn v_item<'mcx>(v: &mut Value<'mcx>) -> Item<'mcx> {
    match take(v) {
        Value::Item(i) => i,
        _ => unreachable!("grammar symbol is not an item"),
    }
}

fn v_opt_item<'mcx>(v: &mut Value<'mcx>) -> Option<Item<'mcx>> {
    match take(v) {
        Value::OptItem(i) => i,
        _ => unreachable!("grammar symbol is not an optional item"),
    }
}

fn v_list<'mcx>(v: &mut Value<'mcx>) -> Option<List<'mcx>> {
    match take(v) {
        Value::List(l) => l,
        _ => unreachable!("grammar symbol is not an item list"),
    }
}

fn v_op(v: &mut Value<'_>) -> ItemType {
    match take(v) {
        Value::Op(t) => t,
        _ => unreachable!("grammar symbol is not an item type"),
    }
}

fn v_int(v: &mut Value<'_>) -> i32 {
    match take(v) {
        Value::Int(n) => n,
        _ => unreachable!("grammar symbol is not an integer"),
    }
}

fn v_bool(v: &mut Value<'_>) -> bool {
    match take(v) {
        Value::Bool(b) => b,
        _ => unreachable!("grammar symbol is not a boolean"),
    }
}

fn list_cell<'mcx>(mcx: Mcx<'mcx>, item: Item<'mcx>) -> PgResult<&'mcx ListCell<'mcx>> {
    Ok(leak_in(alloc_in(mcx, ListCell { item, next: Cell::new(None) })?))
}

fn list_make1<'mcx>(mcx: Mcx<'mcx>, a: Item<'mcx>) -> PgResult<Value<'mcx>> {
    let c = list_cell(mcx, a)?;
    Ok(Value::List(Some(List { head: c, tail: c })))
}

fn lappend<'mcx>(mcx: Mcx<'mcx>, l: Option<List<'mcx>>, a: Item<'mcx>) -> PgResult<Value<'mcx>> {
    let l = l.expect("lappend target is a non-empty list");
    let c = list_cell(mcx, a)?;
    l.tail.next.set(Some(c));
    Ok(Value::List(Some(List { head: l.head, tail: c })))
}

struct Parser<'a, 'e, 's, 'mcx> {
    mcx: Mcx<'mcx>,
    lexer: Lexer<'a, 'mcx>,
    escontext: &'e mut Option<&'s mut SoftErrorContext>,
    result: Option<ParseResult<'mcx>>,
}

impl<'a, 'e, 's, 'mcx> Parser<'a, 'e, 's, 'mcx> {
    /// The grammar actions. Ok(None) = YYABORT after a soft error.
    fn reduce(&mut self, rule: u8, rhs: &mut [Value<'mcx>]) -> PgResult<Option<Value<'mcx>>> {
        use ItemType as T;
        let mcx = self.mcx;
        let item = |i: Item<'mcx>| Value::Item(i);
        let v = match rule {
            2 => {
                let lax = v_bool(&mut rhs[0]);
                let expr = v_item(&mut rhs[1]);
                self.result = Some(ParseResult { expr, lax });
                Value::Empty
            }
            3 => {
                self.result = None;
                Value::Empty
            }
            4 | 5 | 24 | 35 | 65 | 80 => take(&mut rhs[0]),
            6 => Value::Bool(false),
            7 | 8 => Value::Bool(true),
            9 | 33 | 85 => item(make_item_string(mcx, Some(v_str(&mut rhs[0])))?),
            10 => item(make_item_string(mcx, None)?),
            11 => item(make_item_bool(mcx, true)?),
            12 => item(make_item_bool(mcx, false)?),
            13 | 14 | 75 | 82 => item(make_item_numeric(mcx, v_str(&mut rhs[0]))?),
            15 | 34 => item(make_item_variable(mcx, v_str(&mut rhs[0]))?),
            16 => Value::Op(T::Equal),
            17 => Value::Op(T::NotEqual),
            18 => Value::Op(T::Less),
            19 => Value::Op(T::Greater),
            20 => Value::Op(T::LessOrEqual),
            21 => Value::Op(T::GreaterOrEqual),
            22 | 44 | 63 | 66 => take(&mut rhs[1]),
            23 => item(make_item_unary(mcx, T::Exists, v_item(&mut rhs[2]))?),
            25 => {
                let op = v_op(&mut rhs[1]);
                let (l, r) = (v_item(&mut rhs[0]), v_item(&mut rhs[2]));
                item(make_item_binary(mcx, op, Some(l), Some(r))?)
            }
            26 | 27 | 47 | 48 | 49 | 50 | 51 => {
                let op = match rule {
                    26 => T::And,
                    27 => T::Or,
                    47 => T::Add,
                    48 => T::Sub,
                    49 => T::Mul,
                    50 => T::Div,
                    _ => T::Mod,
                };
                let (l, r) = (v_item(&mut rhs[0]), v_item(&mut rhs[2]));
                item(make_item_binary(mcx, op, Some(l), Some(r))?)
            }
            28 => item(make_item_unary(mcx, T::Not, v_item(&mut rhs[1]))?),
            29 => item(make_item_unary(mcx, T::IsUnknown, v_item(&mut rhs[1]))?),
            30 => {
                let (l, r) = (v_item(&mut rhs[0]), v_item(&mut rhs[3]));
                item(make_item_binary(mcx, T::StartsWith, Some(l), Some(r))?)
            }
            31 | 32 => {
                let expr = v_item(&mut rhs[0]);
                let pattern = v_str(&mut rhs[2]);
                let flags = if rule == 32 { Some(v_str(&mut rhs[4])) } else { None };
                match make_item_like_regex(mcx, Some(expr), pattern, flags, self.escontext)? {
                    Some(i) => item(i),
                    None => return Ok(None),
                }
            }
            36 => item(make_item_type(mcx, T::Root)?),
            37 => item(make_item_type(mcx, T::Current)?),
            38 => item(make_item_type(mcx, T::Last)?),
            39 | 54 | 78 => list_make1(mcx, v_item(&mut rhs[0]))?,
            40 | 41 => {
                let l = v_list(&mut list_make1(mcx, v_item(&mut rhs[1]))?);
                lappend(mcx, l, v_item(&mut rhs[3]))?
            }
            42 => {
                let l = v_list(&mut rhs[0]);
                lappend(mcx, l, v_item(&mut rhs[1]))?
            }
            55 | 79 => {
                let l = v_list(&mut rhs[0]);
                lappend(mcx, l, v_item(&mut rhs[2]))?
            }
            43 => {
                let l = v_list(&mut rhs[0]).expect("accessor_expr list is non-empty");
                item(make_item_list(&l.items_in(mcx)?))
            }
            45 => item(make_item_unary(mcx, T::Plus, v_item(&mut rhs[1]))?),
            46 => item(make_item_unary(mcx, T::Minus, v_item(&mut rhs[1]))?),
            52 => item(make_item_binary(mcx, T::Subscript, Some(v_item(&mut rhs[0])), None)?),
            53 => {
                let (l, r) = (v_item(&mut rhs[0]), v_item(&mut rhs[2]));
                item(make_item_binary(mcx, T::Subscript, Some(l), Some(r))?)
            }
            56 => item(make_item_type(mcx, T::AnyArray)?),
            57 => {
                let l = v_list(&mut rhs[1]).expect("index_list is non-empty");
                item(make_index_array(mcx, l.items_in(mcx)?)?)
            }
            58 => {
                let s = v_str(&mut rhs[0]);
                let text = core::str::from_utf8(s).expect("scanner ints are ASCII");
                Value::Int(numutils::pg_strtoint32(text)?)
            }
            59 => Value::Int(-1),
            60 => item(make_any(mcx, 0, -1)?),
            61 => {
                let n = v_int(&mut rhs[2]);
                item(make_any(mcx, n, n)?)
            }
            62 => {
                let (f, l) = (v_int(&mut rhs[2]), v_int(&mut rhs[4]));
                item(make_any(mcx, f, l)?)
            }
            64 => item(make_item_type(mcx, T::AnyKey)?),
            67 => item(make_item_type(mcx, v_op(&mut rhs[1]))?),
            68 => item(make_item_unary(mcx, T::Filter, v_item(&mut rhs[2]))?),
            69 => {
                let l = match v_list(&mut rhs[3]) {
                    Some(l) => l.items_in(mcx)?,
                    None => PgVec::new_in(mcx),
                };
                match l.len() {
                    0 => item(make_item_binary(mcx, T::Decimal, None, None)?),
                    1 => item(make_item_binary(mcx, T::Decimal, Some(l[0]), None)?),
                    2 => item(make_item_binary(mcx, T::Decimal, Some(l[0]), Some(l[1]))?),
                    _ => {
                        return ereturn(
                            self.escontext.as_deref_mut(),
                            None,
                            PgError::error("invalid input syntax for type jsonpath")
                                .with_sqlstate(ERRCODE_SYNTAX_ERROR)
                                .with_detail(
                                    ".decimal() can only have an optional precision[,scale].",
                                ),
                        );
                    }
                }
            }
            70..=74 => {
                let op = match rule {
                    70 => T::Datetime,
                    71 => T::Time,
                    72 => T::TimeTz,
                    73 => T::Timestamp,
                    _ => T::TimestampTz,
                };
                item(make_item_unary_optional(mcx, op, v_opt_item(&mut rhs[3]))?)
            }
            76 | 77 => {
                let num = make_item_numeric(mcx, v_str(&mut rhs[1]))?;
                let op = if rule == 76 { T::Plus } else { T::Minus };
                item(make_item_unary(mcx, op, num)?)
            }
            81 => Value::List(None),
            83 | 86 => Value::OptItem(Some(v_item(&mut rhs[0]))),
            84 | 87 => Value::OptItem(None),
            88 => item(make_item_key(mcx, v_str(&mut rhs[0]))?),
            89..=123 => take(&mut rhs[0]),
            124..=136 => Value::Op(match rule {
                124 => T::Abs,
                125 => T::Size,
                126 => T::Type,
                127 => T::Floor,
                128 => T::Double,
                129 => T::Ceiling,
                130 => T::KeyValue,
                131 => T::Bigint,
                132 => T::Boolean,
                133 => T::Date,
                134 => T::Integer,
                135 => T::Number,
                _ => T::StringFunc,
            }),
            _ => unreachable!("jsonpath grammar has no rule {rule}"),
        };
        Ok(Some(v))
    }
}

enum Outcome {
    Accept,
    Abort,
    Error(&'static str),
}

/// C: parsejsonpath (jsonpath_scan.l) around yyparse (Bison 2.3 yacc.c).
/// Ok(None) = empty input / soft error.
pub fn parsejsonpath<'mcx>(
    mcx: Mcx<'mcx>,
    str: &[u8],
    escontext: Option<&mut SoftErrorContext>,
) -> PgResult<Option<ParseResult<'mcx>>> {
    let mut escontext_ref = escontext;
    let mut parser = Parser {
        mcx,
        lexer: Lexer::new(mcx, str),
        escontext: &mut escontext_ref,
        result: None,
    };

    let mut ss: PgVec<'_, i16> = ::mcx::vec_with_capacity_in(mcx, YYINITDEPTH)?;
    let mut vs: PgVec<'mcx, Value<'mcx>> = ::mcx::vec_with_capacity_in(mcx, YYINITDEPTH)?;
    // yychar: None = YYEMPTY; Some(YYEOF) once the scanner is exhausted.
    let mut yychar: Option<i32> = None;
    let mut yylval: Option<&'mcx [u8]> = None;
    // yytext: span of the most recently scanned token, None at end of input.
    let mut yytext: Option<(usize, usize)> = None;
    let mut state: i32 = 0;
    ss.push(0);
    vs.push(Value::Empty);

    let outcome = 'parse: loop {
        // yysetstate: Bison's stack cannot grow past YYMAXDEPTH entries.
        if ss.len() >= YYMAXDEPTH {
            break 'parse Outcome::Error("memory exhausted");
        }

        let mut n = YYPACT[state as usize] as i32;
        let rule: i32 = 'act: {
            if n == YYPACT_NINF as i32 {
                break 'act YYDEFACT[state as usize] as i32;
            }
            let tok = match yychar {
                Some(t) => t,
                None => {
                    let t = match parser.lexer.next_token(parser.escontext)? {
                        Some(lex) => {
                            yytext = Some((lex.start, lex.end));
                            yylval = lex.value;
                            yytranslate(lex.token)
                        }
                        None => {
                            yytext = None;
                            yylval = None;
                            YYEOF
                        }
                    };
                    yychar = Some(t);
                    t
                }
            };
            n += tok;
            if n < 0 || n > YYLAST || YYCHECK[n as usize] as i32 != tok {
                break 'act YYDEFACT[state as usize] as i32;
            }
            let act = YYTABLE[n as usize];
            if act <= 0 {
                if act == 0 || act == YYTABLE_NINF {
                    break 'parse Outcome::Error("syntax error");
                }
                break 'act -(act as i32);
            }
            if act as i32 == YYFINAL {
                break 'parse Outcome::Accept;
            }
            if tok != YYEOF {
                yychar = None;
            }
            state = act as i32;
            ::mcx::pgvec_reserve(&mut ss, 1)?;
            ::mcx::pgvec_reserve(&mut vs, 1)?;
            ss.push(state as i16);
            vs.push(Value::Str(yylval.take().unwrap_or(&[])));
            continue 'parse;
        };

        if rule == 0 {
            break 'parse Outcome::Error("syntax error");
        }
        let len = YYR2[rule as usize] as usize;
        let base = vs.len() - len;
        let val = match parser.reduce(rule as u8, &mut vs[base..])? {
            Some(v) => v,
            None => break 'parse Outcome::Abort,
        };
        vs.truncate(base);
        ss.truncate(base);
        let top = *ss.last().expect("state 0 is never popped") as i32;
        let lhs = YYR1[rule as usize] as i32 - YYNTOKENS;
        let g = YYPGOTO[lhs as usize] as i32 + top;
        state = if (0..=YYLAST).contains(&g) && YYCHECK[g as usize] as i32 == top {
            YYTABLE[g as usize] as i32
        } else {
            YYDEFGOTO[lhs as usize] as i32
        };
        ::mcx::pgvec_reserve(&mut ss, 1)?;
        ::mcx::pgvec_reserve(&mut vs, 1)?;
        ss.push(state as i16);
        vs.push(val);
    };

    let result = parser.result.take();
    drop(parser);

    match outcome {
        Outcome::Accept => {}
        Outcome::Abort => return Ok(None),
        Outcome::Error(message) => {
            match yytext {
                Some((s, e)) if s < e && e <= str.len() => {
                    jsonpath_yyerror_yytext(escontext_ref.as_deref_mut(), &str[s..e], message)?;
                }
                _ => {
                    jsonpath_yyerror(escontext_ref.as_deref_mut(), str, str.len(), message)?;
                }
            }
            return Ok(None);
        }
    }

    // A scanner soft error stands; C's caller discards the parse
    // (jsonPathFromCstring tests SOFT_ERROR_OCCURRED first).
    if escontext_ref.as_ref().is_some_and(|c| c.error_occurred()) {
        return Ok(None);
    }
    Ok(result)
}
