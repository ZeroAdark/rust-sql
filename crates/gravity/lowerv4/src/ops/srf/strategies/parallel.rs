//! Road `srf.parallel` (H05, R8-L04; L3, registered with its L4 reader, srf.function; cards/M5.md `srf/parallel.rs`,
//! M5-plan §3.7): an SRF step leaves its pipeline parallel when every function of it runs on a pool thread as it does on
//! the leader. The rule is `hir::srf_serial` (M5 L0-d, which source/heap.rs and source/pgrc.rs ask beside
//! `pool_unsafe_site`): a set the iterator emits with no host (the int4 / int8 `generate_series`), parallel safe and not
//! volatile. Every other set stays on the leader: a library entry or an array walk (their per-open contexts are the leader
//! thread's slot bank, M5-HOSTS H-a / H-f), an fmgr or PL call, a table function, a named tuplestore. The per-claim
//! partials merge as any fold's. A FROM function alone is one serial source (a Result), in PG and here: no bar asks for
//! a series split into morsels (inventory row 28). The road's off twin (`gravity_road_off=srf.parallel`) is the serial
//! form with the same rows.
//! Owner M5-SERIES.
use crate::ops::Table;

crate::strategies! {
    /// SRF pipelines stay parallel.
    SRF_PARALLEL = 102, "srf.parallel", Srf;
}

pub(crate) const ROWS: Table = Table { strategies: &[(SRF_PARALLEL, &())], ..crate::ops::NONE };
