use gravity_program::Cmd;
use types_portal::{QueryCompletion, CMDTAG_DELETE, CMDTAG_INSERT, CMDTAG_MERGE, CMDTAG_SELECT, CMDTAG_UPDATE};

pub fn completion(cmd: Cmd, rows_processed: u64) -> QueryCompletion {
    let tag = match cmd {
        Cmd::Select => CMDTAG_SELECT,
        Cmd::Insert => CMDTAG_INSERT,
        Cmd::Update => CMDTAG_UPDATE,
        Cmd::Delete => CMDTAG_DELETE,
        Cmd::Merge => CMDTAG_MERGE,
    };
    QueryCompletion { commandTag: tag, nprocessed: rows_processed }
}
