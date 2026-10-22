//! KeySem's image equalities (decision 24, slice 4): the built-in equality operators whose equal values are one image of
//! each side's type, kept in the key-semantics home (rule RK). Decompose reads them through `Facts::op_image_eq`, which
//! the host (hosts/src/facts.rs) answers from `eq`; a fixture declares its own (`-- imageeq` lines). Read by decompose
//! facts/pinned.rs (a column pinned to one image), facts/passenger.rs (columns equal on every row) and ops/agg keys.
use types_core::Oid;

/// An equality whose equal values are one image, by (operator, type: both sides): int2 / int4 / int8 / oid / date / bool /
/// char, fixed-width words under no collation (D434, D345).
pub const IMAGE_EQ: [(Oid, Oid); 7] = [(94, 21), (96, 23), (410, 20), (607, 26), (1093, 1082), (91, 16), (92, 18)];

/// The integer equalities across widths (int48 / int84 / int24 / int42 / int28 / int82, by (operator, left, right)):
/// equal values are one number, so one image of either side's own type (D345).
pub const INT_CROSS_EQ: [(Oid, Oid, Oid); 6] = [(15, 23, 20), (416, 20, 23), (532, 21, 23), (533, 23, 21), (1862, 21, 20), (1868, 20, 21)];

/// An equality's (left, right) types when its equal values are one image of each side's type: IMAGE_EQ (both sides one
/// type) or INT_CROSS_EQ (two widths; never one type).
pub fn eq(op: Oid) -> Option<(Oid, Oid)> {
    IMAGE_EQ.iter().find(|e| e.0 == op).map(|e| (e.1, e.1)).or_else(|| INT_CROSS_EQ.iter().find(|e| e.0 == op).map(|e| (e.1, e.2)))
}

/// A built-in function that is a dependent form of its one non-constant operand: distinct operand values give distinct
/// results wherever it is defined, so over a key whose group equality is image equality (`eq::group_image`) every row
/// of a group gives one result (functional dependence, AGB-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `x + c`, `x - c`, `c - x` over int2 / int4 / int8 and their mixed widths: strictly monotone in `x` for a non-null
    /// constant `c`.
    IntOffset,
    /// `x * c` over the same widths, for a nonzero constant `c`.
    IntScale,
    /// An int-to-int cast (`int4(int2)` .. `int2(int8)`); `widening` when the result type holds every operand value
    /// (`int4(int2)`, `int8(int4)`, `int8(int2)`).
    IntCast { widening: bool },
    /// `-x` over int2 / int4 / int8.
    IntNegate,
    /// `d + k`, `d - k` over a date and an int4 constant (the int4 is the right argument).
    DateOffset,
    /// `t || c`, `c || t` over text (textcat; anytextcat calls an output function and is stable).
    TextConcat,
}

const INT_OFFSET: [&str; 18] = [
    "int2pl", "int2mi", "int4pl", "int4mi", "int8pl", "int8mi", "int24pl", "int24mi", "int42pl", "int42mi", "int84pl",
    "int84mi", "int48pl", "int48mi", "int82pl", "int82mi", "int28pl", "int28mi",
];
const INT_SCALE: [&str; 9] = ["int2mul", "int4mul", "int8mul", "int24mul", "int42mul", "int84mul", "int48mul", "int82mul", "int28mul"];
const INT_CAST: [&str; 6] = ["i2toi4", "i4toi2", "int84", "int48", "int82", "int28"];
const WIDENING_INT_CAST: [&str; 3] = ["i2toi4", "int48", "int28"];
const INT_NEGATE: [&str; 3] = ["int2um", "int4um", "int8um"];
const DATE_OFFSET: [&str; 2] = ["date_pli", "date_mii"];
const TEXT_CONCAT: [&str; 1] = ["textcat"];

/// Function `f`'s dependent form, by its built-in `prosrc` (pg_proc.dat, through fmgr_core's generated table); `None` for
/// any other function (SQL / user functions included).
pub fn dependent_form(f: Oid) -> Option<Form> {
    let t = fmgr_core::CANONICAL;
    let src = t.binary_search_by_key(&f, |e| e.0).ok().map(|i| t[i].1)?;
    let is = |names: &[&str]| names.contains(&src);
    Some(if is(&INT_OFFSET) {
        Form::IntOffset
    } else if is(&INT_SCALE) {
        Form::IntScale
    } else if is(&INT_CAST) {
        Form::IntCast { widening: is(&WIDENING_INT_CAST) }
    } else if is(&INT_NEGATE) {
        Form::IntNegate
    } else if is(&DATE_OFFSET) {
        Form::DateOffset
    } else if is(&TEXT_CONCAT) {
        Form::TextConcat
    } else {
        return None;
    })
}

/// regress subselect#162 / #163 / #166: text_ops' cross-type members `=(text, name)` / `=(name, text)` (PostgreSQL 12's
/// name / text family, hashed by hashtext and hashname), the built-in cross-type hash equality whose two types' canonical
/// images (typesem `form`: text's payload bytes, name's bytes up to its NUL) are the string's bytes on both sides. The
/// caller (lowerv4 ops/subplan/hashed.rs `test_of`) holds `native` (built-in procs under a collation that compares by
/// bytes, so texteqname is byte equality) and `linear` (a built-in operator). Every other cross-type pair keys by words
/// (`By::Word`) or keeps B2a's name.
pub fn string_pair(lt: u32, rt: u32) -> bool {
    use types_core::catalog::{NAMEOID, TEXTOID};
    matches!((lt, rt), (TEXTOID, NAMEOID) | (NAMEOID, TEXTOID))
}

/// K2: the image a hash join key takes on the bucket road (lowerv4 ops/hashjoin/keys.rs `image`: the build's keys and the
/// probe's alike, one type pair, once any key keys by a range bucket form): `Some(form)`, the Fp128 under the bucket form
/// (`hashjoin::bucket`: a range whose subtype opclass the exact image cannot keep), else `None`, the key's own hash arm.
/// Equal images are hash-equal candidates; the probe's per-candidate range_eq decides. The road takes keys under a
/// deterministic collation only (`bytes_eq`): any other refuses by name.
pub fn bucket_image(typid: Oid, bytes_eq: bool) -> Result<Option<u64>, &'static str> {
    if !bytes_eq {
        return Err("a range key keyed by its bucket form beside a key under a nondeterministic collation");
    }
    Ok(super::hashjoin::bucket(typid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names resolve to exactly the built-ins they stand for (pg_proc.dat OIDs), one each: a renamed or duplicated
    /// `prosrc` in the generated table fails here, not in a plan.
    #[test]
    fn dependent_forms_are_the_builtin_functions() {
        let of = |form: fn(Form) -> bool| -> Vec<u32> {
            let mut v: Vec<u32> = fmgr_core::CANONICAL.iter().map(|e| e.0).filter(|&f| dependent_form(f).is_some_and(form)).collect();
            v.sort_unstable();
            v
        };
        let sorted = |mut v: Vec<u32>| {
            v.sort_unstable();
            v
        };
        let pm = vec![176, 180, 177, 181, 463, 464, 178, 182, 179, 183, 1274, 1275, 1278, 1279, 837, 838, 841, 942]; // alloc: Cold
        assert_eq!(of(|f| f == Form::IntOffset), sorted(pm));
        assert_eq!(of(|f| f == Form::IntScale), sorted(vec![152, 141, 465, 170, 171, 1276, 1280, 839, 943])); // alloc: Cold
        assert_eq!(of(|f| matches!(f, Form::IntCast { .. })), sorted(vec![313, 314, 480, 481, 714, 754])); // alloc: Cold
        assert_eq!(of(|f| f == Form::IntCast { widening: true }), sorted(vec![313, 481, 754])); // alloc: Cold
        assert_eq!(of(|f| f == Form::IntNegate), sorted(vec![212, 213, 462])); // alloc: Cold
        assert_eq!(of(|f| f == Form::DateOffset), sorted(vec![1141, 1142])); // alloc: Cold
        assert_eq!(of(|f| f == Form::TextConcat), vec![1258]); // alloc: Cold
        assert_eq!(dependent_form(316), None, "int4 -> float8 is no form");
    }

    #[test]
    fn same_type_rows_and_cross_width_rows_never_overlap() {
        assert_eq!(eq(96), Some((23, 23)));
        assert_eq!(eq(15), Some((23, 20)));
        assert_eq!(eq(98), None); // texteq: collatable bytes, not an image equality
        assert!(INT_CROSS_EQ.iter().all(|e| e.1 != e.2 && IMAGE_EQ.iter().all(|s| s.0 != e.0)));
    }
}
