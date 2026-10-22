//! Without a pool nothing calls `prime::kick_off` at server ready: the first `limits` (the first range source's bind)
//! probes the device, as before. One test in its own binary: the probe is process-wide.
use gravity_hosts::ops::scan::heap_window::{device_probed, limits};

#[test]
fn without_kick_off_the_first_limits_probes_the_device() {
    assert!(!device_probed());
    let _ = limits(16);
    assert!(device_probed());
}
