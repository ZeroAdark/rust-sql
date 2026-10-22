//! The read-ahead's device probe runs at server ready (`prime::kick_off`), before the first range source binds.
//! One test in its own binary: the probe is process-wide.
use gravity_hosts::ops::scan::heap_window::device_probed;

#[test]
fn kick_off_probes_the_device_before_any_bind() {
    assert!(!device_probed());
    gravity_hosts::ops::scan::pgrc::prime::kick_off(&|_job| false);
    assert!(device_probed(), "server ready probed the device");
}
