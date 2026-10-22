//! Road `sink.ordered` (H05a, R1-L07 R9-L11; driver R1D): a parallel sink pipeline's workers each store into their own
//! out block, and the rows reach the client in claim order (ascending claim: a scan's block / granule order, as serial),
//! streamed while the scan runs. LIMIT stops the claim stream; pool threads are pruned (D234). The sink's IR is the serial
//! sink's (sink.rs): the form is the pipeline's declaration, read by the runner through the program's `OrderedSink`
//! protocol (`gravity_program::v4`; exec/src/v4/m1.rs, M1-HOSTS) over the claim-ordered legs (exec legs.rs, R1D a) and
//! the driver's streaming drain (H05a, M1-DRIVER). Budget 160 lines.
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Parallel;

use crate::face::end::SinkKind;
use crate::frame::px::Px;

crate::strategies! {
    /// Per-worker out blocks in claim order (driver R1D).
    SINK_ORDERED = 33, "sink.ordered", RowsOut;
}

/// The runner's `OrderedSink` protocol (exec/src/v4/m1.rs) is a no-op prepare; the driver's claim-order drain streams the
/// rows (H05a, R1D b/c).
pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(crate::face::strategy::SINK_ORDERED, &())], ..crate::ops::NONE };

/// The form pipeline `k`'s sink takes: `Ordered` (declaring the pipeline's `OrderedSink` protocol) when the pipeline ends
/// in the sink and may run parallel, else `Serial`. A serial pipeline's order is already the claim order.
pub(crate) fn form(px: &mut Px<'_, '_>, k: usize) -> SinkKind {
    let d = px.gx.pipelines()[k].desc;
    if !crate::frame::iface::ends_in_sink(px.gx, k) || !matches!(d.parallel, Parallel::Eligible) || !SinkKind::Ordered.on() {
        return SinkKind::Serial;
    }
    px.declare_proto(Proto::new(ProtoKind::OrderedSink, k as u16));
    SinkKind::Ordered
}
