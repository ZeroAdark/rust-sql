use gravity_driver::grant_ledger::{mode, on, Mode};

#[test]
fn s7c_an_unset_ledger_switch_reports_as_servers_of_record_do() {
    std::env::remove_var("PGRUST_GRAVITY_LEDGER");
    assert_eq!(mode(), Mode::Report);
    assert!(on());
}
