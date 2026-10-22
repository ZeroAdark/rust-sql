//! M4-TRIG (cards/M4.md `write/triggers.rs`, kind `write.transition`): the trigger plan over a served INSERT fixture
//! (oltp-029) whose target's trigger facts are rewritten. A trigger that fires on INSERT (BEFORE / AFTER ROW, statement,
//! transition capture, RI's internal AFTER ROW) or facts the source could not answer take the plan: the program serves
//! and declares `ProtoKind::Transition`, and the host fires them. A trigger on another command only takes none (G4).
use gravity_lowerv4_it as common;

use gravity_program::protos::ProtoKind;

const NONE: &str = "before=- after=- instead=- stmt=- trans=- internal=- defer=- rules=f";

fn fixture() -> String {
    let p = common::plans_dir().join("oltp-029.plan");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert_eq!(text.matches(NONE).count(), 1, "oltp-029's target carries one known-empty trigext line");
    text
}

/// The fixture's lowering with its target's trigger facts `set` (a trigext line's fields, or the whole line dropped when
/// `None`): whether its program declares write.transition's protocol, or its refusal.
fn transition(set: Option<&str>) -> Result<bool, String> {
    let text = fixture();
    let text = match set {
        Some(s) => text.replace(NONE, s),
        None => text.lines().filter(|l| !l.starts_with("-- trigext ")).collect::<Vec<_>>().join("\n"),
    };
    match common::lower_fixture(&text) {
        Some(Ok(l)) => Ok(l.descs.v4.protos.iter().any(|p| p.kind == ProtoKind::Transition)),
        Some(Err(e)) => Err(format!("{e}")),
        None => Err("decompose refused the fixture".into()),
    }
}

#[test]
fn the_fixture_serves_without_triggers() {
    assert_eq!(transition(Some(NONE)), Ok(false));
}

/// Every trigger kind that fires on INSERT takes the trigger plan and serves; so do unknown facts (the host binds the
/// relation's own descriptor).
#[test]
fn an_insert_trigger_takes_the_trigger_plan() {
    for set in [
        "before=i after=- instead=- stmt=- trans=- internal=- defer=- rules=f",
        "before=- after=i instead=- stmt=- trans=- internal=- defer=- rules=f",
        "before=- after=- instead=- stmt=i trans=- internal=- defer=- rules=f",
        "before=- after=iu instead=- stmt=iu trans=i internal=- defer=- rules=f",
        "before=- after=iu instead=- stmt=- trans=- internal=iu defer=iu rules=f",
    ] {
        assert_eq!(transition(Some(set)), Ok(true), "{set}");
    }
    assert_eq!(transition(None), Ok(true), "unknown trigger facts");
}

/// G4: a trigger on another command only (an UPDATE / DELETE trigger, a referenced table's RI actions) takes no plan.
#[test]
fn an_unrelated_command_trigger_takes_none() {
    for set in [
        "before=u after=ud instead=- stmt=ud trans=ud internal=- defer=- rules=f",
        "before=- after=ud instead=- stmt=- trans=- internal=ud defer=- rules=f",
    ] {
        assert_eq!(transition(Some(set)), Ok(false), "{set}");
    }
}
