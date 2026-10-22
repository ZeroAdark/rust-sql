
use crate::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqAlike {
    Image,
    Text { pad: bool },
    Numeric,
    Float,
    Apart,
}

/// The class of a built-in type (PostgreSQL 18.6's pg_type), `None` for a type the table does not list.
pub const fn builtin(typid: Oid) -> Option<EqAlike> {
    use EqAlike::*;
    Some(match typid {
        16 | 17 | 18 | 20 | 21 | 23 | 24 | 26 | 27 | 28 | 29 | 650 | 774 | 790 | 829 | 869 | 1082 | 1083 | 1114 | 1184 | 1266
        | 1560 | 1562 | 2202..=2206 | 2950 | 3220 | 3734 | 3769 | 4089 | 4096 | 4191 | 5069 => Image,
        19 | 25 | 1043 => Text { pad: false },
        1042 => Text { pad: true },
        1700 => Numeric,
        700 | 701 => Float,
        22 | 30 | 114 | 142 | 194 | 600..=604 | 718 | 1002 | 1003 | 1005 | 1007 | 1009 | 1014 | 1015 | 1016 | 1021 | 1022
        | 1028 | 1033 | 1034 | 1186 | 2249 | 2277 | 2970 | 3361 | 3402 | 3614 | 3615 | 3802 | 3904 | 3906 | 3908
        | 3910 | 3912 | 3926 | 4451 | 4532 | 5017 | 5038 => Apart,
        _ => return None,
    })
}

const ARRAY_ELEMS: [(Oid, Oid); 16] = [
    (1000, 16),   // _bool
    (1001, 17),   // _bytea
    (1002, 18),   // _char
    (1003, 19),   // _name
    (1005, 21),   // _int2
    (1007, 23),   // _int4
    (1009, 25),   // _text
    (1014, 1042), // _bpchar
    (1015, 1043), // _varchar
    (1016, 20),   // _int8
    (1028, 26),   // _oid
    (1115, 1114), // _timestamp
    (1182, 1082), // _date
    (1185, 1184), // _timestamptz
    (1231, 1700), // _numeric
    (2951, 2950), // _uuid
];

pub fn array_elem(typid: Oid) -> Option<Oid> {
    ARRAY_ELEMS.binary_search_by_key(&typid, |p| p.0).ok().map(|i| ARRAY_ELEMS[i].1)
}

pub const fn class_alike(class: EqAlike, typmod: i32, coll_bytes_eq: bool, float_hidden: bool) -> bool {
    match class {
        EqAlike::Image => true,
        EqAlike::Text { pad } => coll_bytes_eq && (!pad || typmod > 0),
        EqAlike::Numeric => typmod >= 0,
        EqAlike::Float => float_hidden,
        EqAlike::Apart => false,
    }
}

pub fn alike(typid: Oid, typmod: i32, coll_bytes_eq: bool, float_hidden: bool, other: impl FnOnce(Oid) -> Option<EqAlike>) -> bool {
    let typid = match array_elem(typid) {
        Some(elem) => elem,
        None => typid,
    };
    match builtin(typid) {
        Some(class) => class_alike(class, typmod, coll_bytes_eq, float_hidden),
        None => other(typid).is_some_and(|class| class_alike(class, typmod, coll_bytes_eq, float_hidden)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_array_list_is_sorted_and_its_elements_are_listed() {
        assert!(ARRAY_ELEMS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(ARRAY_ELEMS.iter().all(|&(_, e)| builtin(e).is_some_and(|c| c != EqAlike::Apart)));
    }

    #[test]
    fn members_print_alike_by_class() {
        let none = |_| None;
        assert!(alike(23, -1, false, false, none));
        assert!(alike(1007, -1, false, false, none)); // int4[] reads int4
        assert!(alike(25, -1, true, false, none));
        assert!(!alike(25, -1, false, false, none)); // text under a nondeterministic collation
        assert!(!alike(1042, -1, true, false, none) && alike(1042, 8, true, false, none));
        assert!(!alike(1700, -1, true, false, none) && alike(1700, (12 << 16 | 2) + 4, true, false, none));
        assert!(!alike(1231, -1, true, false, none)); // numeric[] without a typmod
        assert!(!alike(701, -1, true, false, none) && alike(701, -1, true, true, none));
        assert!(!alike(1186, -1, true, false, none) && !alike(3802, -1, true, false, none));
        assert!(!alike(90_000, -1, true, false, none) && alike(90_000, -1, true, false, |_| Some(EqAlike::Image)));
    }
}
