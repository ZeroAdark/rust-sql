//! Default-empty hooks an extension linked into the server installs into the instance aux.

#[derive(Clone, Debug, PartialEq)]
pub enum ExtValue {
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExternalSample {
    pub term: Option<u64>,
    pub durable_lsn: Option<u64>,
    pub log_end_lsn: Option<u64>,
    pub ext: Vec<(String, ExtValue)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LivenessSample {
    pub seq: u64,
    pub lsn: u64,
    pub acked_ms: Option<i64>,
    pub wedged: bool,
}

seam_core::tap!(pub fn startup_check() -> Result<(), String>);

seam_core::tap!(pub fn external_sample() -> ExternalSample);

seam_core::tap!(pub fn liveness() -> Option<LivenessSample>);

seam_core::tap!(pub fn liveness_committed(seq: u64, lsn: u64));

seam_core::tap!(pub fn promote_precheck(expected_term: Option<u64>) -> Result<(), String>);

seam_core::tap!(pub fn fence_changed(fenced: bool));
