//! D1272 (M6-AGG, main 09-27 19:29Z; landed code V4R-375): a keyed hosted table's open (fold/hosted.rs `home_open`) left
//! by the exit with the kernel's raw ERROR status when `m6_agg_home_open` failed. The kernel stashes PostgreSQL's error,
//! but only the HOST raise collects the stash into the driver's error, so the client read "program reported an error
//! without a message". It now leaves by the HOST bit, as `home` does. The crate rig prepares no aggregate arena (its
//! handle word is 0: the kernel's stale-arena error), so m6x-055 (two DISTINCT arguments grouped, a count and a sum: host-
//! sorted since D1210, a keyed hosted table) reaches exactly that failure: the outcome is the HOST raise, never a
//! message-less error. (The witness was m6x-039, two count(DISTINCT) arguments, until the tagged pair table served it.)
use gravity_lowerv4_it as common;

use gravity_abi::status::raise;

#[test]
fn a_failed_hosted_table_open_raises_host() {
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    let text = std::fs::read_to_string(common::milestone_dir(6).join("m6x-055.plan")).expect("plan fixture m6x-055");
    let host = format!("Raised({})", raise::bit(raise::HOST));
    for mode in common::MODES {
        let got = common::run_fixture(&text, mode).expect("decompose takes m6x-055");
        match got {
            Ok(common::multiset::Outcome::Error { message, .. }) => {
                assert!(!message.contains("without a message"), "{}: a message-less error: {message}", common::mode_name(mode));
                assert_eq!(message, host, "{}: the HOST raise", common::mode_name(mode));
            }
            other => panic!("{}: {other:?} (the rig has no aggregate arena: the table's open must raise HOST)", common::mode_name(mode)),
        }
    }
}
