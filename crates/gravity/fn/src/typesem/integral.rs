//! typesem::integral: the integer facts a fast path reads instead of a type or function OID list (aggtrans overfit
//! OIDW-70 / OIDW-71, main's go of 10-04 under DEPENDENCIES rule 4; semantics' home): a type's integral width
//! (`int_width`) and what a built-in integer operator's function computes (`int_arith`).

use types_core::catalog::{INT2OID, INT4OID, INT8OID};

type Oid = u32;

/// An integer operator function's operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
}

/// What a built-in integer arithmetic function computes: `op` over its operands sign-extended from `left` / `right`
/// bytes, exact, raising its result type's "out of range" (22003) past `result` bytes, the wider operand's width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntArith {
    pub op: IntOp,
    pub left: u8,
    pub right: u8,
    pub result: u8,
}

/// The integral-width property: the width in bytes (2, 4, 8) of a two's-complement integer type (smallint, integer,
/// bigint) whose arithmetic raises past its range; a domain over one reads as its base type (getBaseType, as PostgreSQL
/// resolves an operator over a domain); `None` for any other type. The three answer without a catalog read; another
/// type is read only where the catalog is installed (a crate rig has none), and a failed read is `None`.
pub fn int_width(typid: Oid) -> Option<u8> {
    fn of(t: Oid) -> Option<u8> {
        match t {
            INT2OID => Some(2),
            INT4OID => Some(4),
            INT8OID => Some(8),
            _ => None,
        }
    }
    of(typid).or_else(|| {
        let base = syscache_seams::pg_type_base_shape::is_installed().then(|| lsyscache::getBaseType(typid).ok()).flatten()?;
        (base != typid).then(|| of(base)).flatten()
    })
}

/// What built-in function `funcid` computes when it is an integer `+`, `-` or `*`: its pg_proc row's C routine names
/// the operation and the operand widths (`int4pl`, `int48mul`, `int28mi`, as int.c / int8.c spell them), and every
/// one is exact and raises 22003 past its result type, the wider operand's. `None` for any other function.
pub fn int_arith(funcid: Oid) -> Option<IntArith> {
    let (left, right, op) = int_routine(funcid)?;
    let op = match op {
        "pl" => IntOp::Add,
        "mi" => IntOp::Sub,
        "mul" => IntOp::Mul,
        _ => return None,
    };
    Some(IntArith { op, left, right, result: left.max(right) })
}

/// A built-in integer division or remainder function: `div` (`int2div` .. `int84div`: `/`, truncating toward zero) or
/// not (`int2mod` / `int4mod` / `int8mod`: `%`, the dividend's sign), over operands of `left` / `right` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntDivMod {
    pub div: bool,
    pub left: u8,
    pub right: u8,
}

/// What built-in function `funcid` computes when it is an integer `/` or `%`: its pg_proc row's C routine names the
/// operation and the operand widths as int.c / int8.c spell them (`int4div`, `int48div`, `int8mod`; a `mod(x, y)` call
/// names the same routine as `x % y`). `None` for any other function.
pub fn int_divmod(funcid: Oid) -> Option<IntDivMod> {
    let (left, right, op) = int_routine(funcid)?;
    let div = match op {
        "div" => true,
        "mod" => false,
        _ => return None,
    };
    Some(IntDivMod { div, left, right })
}

/// An `int<widths><op>` C routine's operand widths in bytes and its operation suffix, as int.c / int8.c name them.
fn int_routine(funcid: Oid) -> Option<(u8, u8, &'static str)> {
    let rest = crate::row_of(funcid)?.name.strip_prefix("int")?;
    let (d, op) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
    let (left, right) = match d {
        "2" => (2, 2),
        "4" => (4, 4),
        "8" => (8, 8),
        "24" => (2, 4),
        "42" => (4, 2),
        "28" => (2, 8),
        "82" => (8, 2),
        "48" => (4, 8),
        "84" => (8, 4),
        _ => return None,
    };
    Some((left, right, op))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in `+` / `-` / `*` over smallint / integer / bigint and nothing else: 9 width pairs by 3 operations,
    /// each at the wider operand's width (a cast `int48`, `int8_numeric`, `int4inc` or `int8pl_inet` is none).
    #[test]
    fn int_arith_names_exactly_the_integer_operators() {
        let found: Vec<(u32, IntArith)> = crate::ROWS.iter().filter_map(|r| int_arith(r.oid).map(|a| (r.oid, a))).collect();
        assert_eq!(found.len(), 27, "{found:?}");
        let shapes: std::collections::BTreeSet<_> = found.iter().map(|(_, a)| (a.op, a.left, a.right)).collect();
        assert_eq!(shapes.len(), 27, "one function per operation and width pair");
        assert!(found.iter().all(|(_, a)| a.result == a.left.max(a.right)));
        assert_eq!(int_arith(177), Some(IntArith { op: IntOp::Add, left: 4, right: 4, result: 4 }), "int4pl");
        assert_eq!(int_arith(1280), Some(IntArith { op: IntOp::Mul, left: 4, right: 8, result: 8 }), "int48mul");
        assert_eq!(int_arith(942), Some(IntArith { op: IntOp::Sub, left: 2, right: 8, result: 8 }), "int28mi");
        for none in crate::ROWS.iter().filter(|r| ["int48", "int8_numeric", "int4inc", "int8pl_inet", "int4um"].contains(&r.name)) {
            assert_eq!(int_arith(none.oid), None, "{}", none.name);
        }
    }

    /// Every built-in integer `/` and `%`: the 9 width pairs' divisions and the 3 same-width remainders, the remainders
    /// twice each (the `%` operator's function and `mod(x, y)` name one routine); nothing else.
    #[test]
    fn int_divmod_names_exactly_the_integer_divisions_and_remainders() {
        let found: std::collections::BTreeMap<u32, IntDivMod> = crate::ROWS.iter().filter_map(|r| int_divmod(r.oid).map(|a| (r.oid, a))).collect();
        let div: Vec<u32> = found.iter().filter(|(_, a)| a.div).map(|(&o, _)| o).collect();
        let rem: Vec<u32> = found.iter().filter(|(_, a)| !a.div).map(|(&o, _)| o).collect();
        assert_eq!(div, [153, 154, 172, 173, 466, 840, 948, 1277, 1281]);
        assert_eq!(rem, [155, 156, 940, 941, 945, 947]);
        assert_eq!(int_divmod(1281), Some(IntDivMod { div: true, left: 4, right: 8 }), "int48div");
        assert_eq!(int_divmod(940), Some(IntDivMod { div: false, left: 2, right: 2 }), "mod(int2, int2)");
        assert_eq!(int_divmod(177), None, "int4pl");
    }

    /// The raise-free rules of an integer `/` and `%` (`types_core::quiet::int_div` / `int_mod`) name exactly the
    /// functions whose C routine is an integer division or remainder: `mod(x, y)` is as quiet as `x % y`, the same routine.
    #[test]
    fn the_quiet_divisor_rules_name_the_catalog_divisions_and_remainders() {
        for r in crate::ROWS.iter() {
            let d = int_divmod(r.oid);
            assert_eq!(types_core::quiet::int_div(r.oid), d.is_some_and(|a| a.div), "{} {}", r.oid, r.name);
            assert_eq!(types_core::quiet::int_mod(r.oid), d.is_some_and(|a| !a.div), "{} {}", r.oid, r.name);
        }
    }

    /// smallint / integer / bigint by their widths; numeric, oid and money (integers underneath, other arithmetic) are not
    /// integral here, nor is any type in a rig without the catalog.
    #[test]
    fn int_width_of_the_integer_types_only() {
        use types_core::catalog::{CASHOID, NUMERICOID, OIDOID};
        assert_eq!([INT2OID, INT4OID, INT8OID].map(int_width), [Some(2), Some(4), Some(8)]);
        assert_eq!([NUMERICOID, OIDOID, CASHOID].map(int_width), [None; 3]);
    }
}
