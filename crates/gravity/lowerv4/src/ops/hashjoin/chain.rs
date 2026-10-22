//! Kind `join.chain` (several probes and nests on one outer pipeline; the chain face agreed with M8, C1-C10 / R1-R6;
//! M7-plan §3.7, §5.1) (cards/M7.md, M7-changes.md). Owner M7-PROBE. Nothing here lowers code: the pipeline composes its
//! join steps by asking each op's own kind for its step maker (C1: a probe's is join.inner's, a nest's is M8's), so the row
//! declares the kind served once every step is (the keyed.gsets precedent). Resume is the frame's (C2: the innermost set
//! offset word re-enters first, D688), each probe reads its own build (`probe_inner.rs` `consumed`), an outer-first probe
//! ends the pipeline early only over a raise-free prefix (C8: `ops_raise_free` takes no join op), and each lazy build waits
//! for the first row that reaches its own probe (C9: `Px::lazy` holds one entry per probe).
use crate::face::kinds::KindId;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinChain, &())], ..crate::ops::NONE };
