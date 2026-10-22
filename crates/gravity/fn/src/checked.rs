//! Flag-returning arithmetic for the kernels' inline forms (docs/gravity/expr.md §2.4): value +
//! flag, no branch on the flag, a masked divisor so a speculative division never traps. The
//! flag says "this row would have raised"; the replay of a dirty window calls the shim (the typed
//! body) serially and raises PostgreSQL's own error at the first dirty row, so one bit per row is
//! all the vector path carries. Each form is differential-tested against the typed body.

macro_rules! int_ops {
    ($($t:ident: $add:ident $sub:ident $mul:ident $div:ident $mod:ident $neg:ident;)*) => {$(
        #[inline(always)]
        pub fn $add(a: $t, b: $t) -> ($t, bool) {
            a.overflowing_add(b)
        }
        #[inline(always)]
        pub fn $sub(a: $t, b: $t) -> ($t, bool) {
            a.overflowing_sub(b)
        }
        #[inline(always)]
        pub fn $mul(a: $t, b: $t) -> ($t, bool) {
            a.overflowing_mul(b)
        }
        #[inline(always)]
        pub fn $div(a: $t, b: $t) -> ($t, bool) {
            let zero = b == 0;
            let (q, over) = a.overflowing_div(if zero { 1 } else { b });
            (q, zero | over)
        }
        #[inline(always)]
        pub fn $mod(a: $t, b: $t) -> ($t, bool) {
            let zero = b == 0;
            (a.wrapping_rem(if zero { 1 } else { b }), zero)
        }
        #[inline(always)]
        pub fn $neg(a: $t) -> ($t, bool) {
            a.overflowing_neg()
        }
    )*};
}

int_ops! {
    i16: add_i16 sub_i16 mul_i16 div_i16 mod_i16 neg_i16;
    i32: add_i32 sub_i32 mul_i32 div_i32 mod_i32 neg_i32;
    i64: add_i64 sub_i64 mul_i64 div_i64 mod_i64 neg_i64;
}

#[inline(always)]
pub fn add_f64(a: f64, b: f64) -> (f64, bool) {
    let r = a + b;
    (r, r.is_infinite() & !a.is_infinite() & !b.is_infinite())
}

#[inline(always)]
pub fn sub_f64(a: f64, b: f64) -> (f64, bool) {
    let r = a - b;
    (r, r.is_infinite() & !a.is_infinite() & !b.is_infinite())
}

#[inline(always)]
pub fn mul_f64(a: f64, b: f64) -> (f64, bool) {
    let r = a * b;
    (r, (r.is_infinite() & !a.is_infinite() & !b.is_infinite()) | (r == 0.0 && a != 0.0 && b != 0.0))
}

#[inline(always)]
pub fn div_f64(a: f64, b: f64) -> (f64, bool) {
    let zero = b == 0.0 && !a.is_nan();
    let r = a / if zero { 1.0 } else { b };
    (r, zero | (r.is_infinite() & !a.is_infinite()) | (r == 0.0 && a != 0.0 && !b.is_infinite()))
}

#[inline(always)]
pub fn add_f32(a: f32, b: f32) -> (f32, bool) {
    let r = a + b;
    (r, r.is_infinite() & !a.is_infinite() & !b.is_infinite())
}

#[inline(always)]
pub fn mul_f32(a: f32, b: f32) -> (f32, bool) {
    let r = a * b;
    (r, (r.is_infinite() & !a.is_infinite() & !b.is_infinite()) | (r == 0.0 && a != 0.0 && b != 0.0))
}

#[inline(always)]
pub fn div_f32(a: f32, b: f32) -> (f32, bool) {
    let zero = b == 0.0 && !a.is_nan();
    let r = a / if zero { 1.0 } else { b };
    (r, zero | (r.is_infinite() & !a.is_infinite()) | (r == 0.0 && a != 0.0 && !b.is_infinite()))
}

#[inline(always)]
pub fn set_flag(word: &mut u64, row: usize, flag: bool, live: bool) {
    *word |= ((flag & live) as u64) << row;
}

#[inline(always)]
pub fn first_dirty(word: u64) -> Option<usize> {
    (word != 0).then(|| word.trailing_zeros() as usize)
}
