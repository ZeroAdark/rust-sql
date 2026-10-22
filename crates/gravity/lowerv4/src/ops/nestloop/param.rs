//! Kind `nest.param` (NestLoopParam: the param words, an index inner driven per outer row; L0-w's `supply`; L2, B1)
//! (cards/M8.md, M8-changes.md §1). A NestLoopParam is written per outer row by nest.loop (nest/loop.rs, params.rs
//! `store`) into its frame word pair (params.rs `word`), and read where its readers read it: an expression's operand
//! (frame/hoist.rs, never hoisted) and a bare index / bitmap / tid / sample key at every (re-)open of the inner's source
//! (frame/site.rs `EXEC_KEY`). This row marks the kind built for those readers (`params::need`); a key or bound computed
//! from parameters still refuses (`params::key`, D1016). Owner M8-NEST.
use crate::face::kinds::KindId;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestParam, &())], ..crate::ops::NONE };
