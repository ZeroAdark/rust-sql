//! gravity_fn's run-time inputs home (rule C8): the environment reads the function layer makes. gravity_fn sits below
//! lowerv4, exec, hosts and the driver, so it reaches none of their inputs homes; this is its own, as hosts/src/inputs.rs
//! is the hosts'. Plain readers: each reads at the point its caller always did; none is a recorded program input.

/// `PGRUST_GRAVITY_SCRIBBLE=1` (test only, D202): result_life's scribble of `UntilNextCall` results is on. Read by
/// `result_life::scribble::on` once per process, as before.
pub fn scribble() -> bool {
    std::env::var("PGRUST_GRAVITY_SCRIBBLE").is_ok_and(|v| v == "1")
}
