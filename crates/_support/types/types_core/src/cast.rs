use crate::catalog::{INT2OID, INT4OID, INT8OID, NUMERICOID};

pub const F_INT4_INT2: u32 = 313;
pub const F_INT2_INT4: u32 = 314;
pub const F_INT4_INT8: u32 = 480;
pub const F_INT8_INT4: u32 = 481;
pub const F_INT2_INT8: u32 = 714;
pub const F_INT8_INT2: u32 = 754;
pub const F_NUMERIC_INT4: u32 = 1740;
pub const F_NUMERIC_INT8: u32 = 1781;
pub const F_NUMERIC_INT2: u32 = 1782;

pub const fn int_digits(typid: u32) -> Option<u32> {
    match typid {
        INT2OID => Some(5),
        INT4OID => Some(10),
        INT8OID => Some(19),
        _ => None,
    }
}

const CASTS: [(u32, u32, u32); 9] = [
    (F_INT4_INT2, INT2OID, INT4OID),
    (F_INT2_INT4, INT4OID, INT2OID),
    (F_INT4_INT8, INT8OID, INT4OID),
    (F_INT8_INT4, INT4OID, INT8OID),
    (F_INT2_INT8, INT8OID, INT2OID),
    (F_INT8_INT2, INT2OID, INT8OID),
    (F_NUMERIC_INT4, INT4OID, NUMERICOID),
    (F_NUMERIC_INT8, INT8OID, NUMERICOID),
    (F_NUMERIC_INT2, INT2OID, NUMERICOID),
];

pub fn exact(funcid: u32) -> Option<(u32, u32)> {
    CASTS
        .iter()
        .find(|c| c.0 == funcid)
        .map(|c| (c.1, c.2))
        .filter(|&(s, t)| int_digits(s).is_some() && (int_digits(t).is_some() || t == NUMERICOID))
}

pub fn exact_to_numeric(funcid: u32) -> Option<u32> {
    exact(funcid).filter(|c| c.1 == NUMERICOID).map(|c| c.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_casts_are_exact_and_name_their_source() {
        for f in [F_INT4_INT2, F_INT2_INT4, F_INT4_INT8, F_INT8_INT4, F_INT2_INT8, F_INT8_INT2] {
            assert!(exact(f).is_some_and(|(s, t)| int_digits(s).is_some() && int_digits(t).is_some()), "{f}");
            assert_eq!(exact_to_numeric(f), None, "{f}: not to numeric");
        }
        assert_eq!(exact_to_numeric(F_NUMERIC_INT4), Some(INT4OID));
        assert_eq!(exact_to_numeric(F_NUMERIC_INT8), Some(INT8OID));
        assert_eq!(exact_to_numeric(F_NUMERIC_INT2), Some(INT2OID));
        assert_eq!((F_INT4_INT2, F_INT8_INT2, F_INT8_INT4, F_NUMERIC_INT4, F_NUMERIC_INT8, F_NUMERIC_INT2), (313, 754, 481, 1740, 1781, 1782));
        assert!(exact(1744).is_none() && exact(317).is_none() && exact(1251).is_none());
        assert_eq!((int_digits(21), int_digits(23), int_digits(20), int_digits(1700)), (Some(5), Some(10), Some(19), None));
    }
}
