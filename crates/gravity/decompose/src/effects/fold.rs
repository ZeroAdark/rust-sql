//! The aggregate folds' raise-by-order classes: which folds may raise by their partial sums, so an input order other than
//! PostgreSQL's can meet an overflow it does not (or miss one it meets). The order-free rule itself is facts/order.rs
//! `order_free`; `facts::order::license_raising` asks these for its recheck licence.
use crate::ops::shared::agg_class::Class;

/// Whether class `c`'s fold may raise by its partial sums (an overflow one input order meets and another does not):
/// float, money and interval sums and averages, float variances; a bit string AND / OR / XOR (its size mismatch raised
/// at the first unequal row, which input order decides beside another row's error). A caller that folds one out of PostgreSQL's order asks
/// a recheck licence for it (`effects::licence` with `Departure::Ahead`).
pub const fn raises_by_order(c: Class) -> bool {
    matches!(c, Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) | Class::SumMoney | Class::SumInterval | Class::AvgInterval | Class::BitVar(_))
}

/// D2597: whether class `c` raises by its partial sums with no guard of its own for a row order PG's scan does not take:
/// money and interval sums and averages (PG's own transfns, hosted). The float folds raise wherever some order could
/// overflow (lowerv4 class_float's `risk`), so their re-run answers; these keep a plain scan's forward walk (lowerv4
/// heap_pages: PG's scan order decides `9e16 + 9e16 - 9e16`'s money out of range). Beside a DISTINCT or in a sorted group
/// PG's own order among tied rows is its sort's.
pub const fn scan_raises(c: Class) -> bool {
    matches!(c, Class::SumMoney | Class::SumInterval | Class::AvgInterval)
}
