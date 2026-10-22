use gravity_program::Cmd;

pub const TRIG_INSERT: u8 = 1;
pub const TRIG_UPDATE: u8 = 2;
pub const TRIG_DELETE: u8 = 4;

/// A written relation's trigger kinds as command bits (`TRIG_*`, the relcache trigger descriptor's flags per
/// trigger); `known` false when the source has none. The write gate reads it through `refusal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TrigExt {
    pub before_row: u8,
    pub after_row: u8,
    pub instead_row: u8,
    pub statement: u8,
    pub transition: u8,
    pub internal: u8,
    pub deferrable: u8,
    pub known: bool,
}

pub fn cmd_bits(cmd: Cmd) -> u8 {
    match cmd {
        Cmd::Insert => TRIG_INSERT,
        Cmd::Update => TRIG_UPDATE,
        Cmd::Delete => TRIG_DELETE,
        Cmd::Merge => TRIG_INSERT | TRIG_UPDATE | TRIG_DELETE,
        Cmd::Select => 0,
    }
}

impl TrigExt {
    pub const UNKNOWN: TrigExt = TrigExt { before_row: 0, after_row: 0, instead_row: 0, statement: 0, transition: 0, internal: 0, deferrable: 0, known: false };
    pub const NONE: TrigExt = TrigExt { known: true, ..TrigExt::UNKNOWN };

    /// The trigger property the command's write cannot serve, by name (rung 10 step 7: row, statement, transition-table and
    /// deferrable constraint triggers are served, a MERGE target's statement triggers and transition tables too, fired by
    /// the host over the statement's actions (H-f, V4R-257), and a MERGE target's BEFORE ROW triggers (V4R-313, the actions'
    /// prologue: hosts trigger_before.rs); INSTEAD OF is not).
    pub fn refusal(&self, cmd: Cmd) -> Option<&'static str> {
        self.refusal_on(cmd, false)
    }

    /// `view`: the target is a view, whose INSTEAD OF row trigger is the write itself (member `TargetView`), not a refusal.
    pub fn refusal_on(&self, cmd: Cmd, view: bool) -> Option<&'static str> {
        let c = cmd_bits(cmd);
        if !self.known {
            Some("a written relation with triggers (its trigger facts unknown)")
        } else if self.instead_row & c != 0 && !view {
            Some("an INSTEAD OF row trigger on the written relation")
        } else {
            None
        }
    }

    pub fn fires(&self, cmd: Cmd) -> bool {
        self.after_row & cmd_bits(cmd) != 0
    }

    pub fn fires_before(&self, cmd: Cmd) -> bool {
        self.before_row & cmd_bits(cmd) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V4R-257: a MERGE target's statement triggers and transition tables are the host's (H-f); V4R-313: its BEFORE ROW
    /// triggers too (the actions' prologue). INSTEAD OF and unknown facts stay refused.
    #[test]
    fn a_merge_target_refuses_only_instead_of_and_unknown_facts() {
        let stmt = TrigExt { statement: TRIG_UPDATE, transition: TRIG_INSERT, after_row: TRIG_DELETE, ..TrigExt::NONE };
        assert_eq!(stmt.refusal(Cmd::Merge), None);
        let before = TrigExt { before_row: TRIG_INSERT | TRIG_UPDATE | TRIG_DELETE, ..TrigExt::NONE };
        assert_eq!(before.refusal(Cmd::Merge), None);
        assert_eq!(before.refusal(Cmd::Update), None);
        assert!(TrigExt { instead_row: TRIG_UPDATE, ..TrigExt::NONE }.refusal(Cmd::Merge).is_some());
        assert!(TrigExt::UNKNOWN.refusal(Cmd::Merge).is_some());
    }
}
