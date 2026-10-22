//! The cost home's pgrc granule (the run-time filter's set cap, JOIN-07) is the format's.
#[test]
fn granule_rows_match_the_format() {
    assert_eq!(gravity_program::cost::PGRC_GRANULE_ROWS, pgrc_format::geom::GRANULE_ROWS as u64);
}

#[test]
fn set_cap_is_the_probe_granules_at_least_the_floor() {
    use gravity_program::cost::{rtfilter_set_max, RTFILTER_SET_MAX};
    assert_eq!(rtfilter_set_max(None), RTFILTER_SET_MAX);
    assert_eq!(rtfilter_set_max(Some(0)), RTFILTER_SET_MAX);
    assert_eq!(rtfilter_set_max(Some(64 * 8192)), RTFILTER_SET_MAX);
    assert_eq!(rtfilter_set_max(Some(64 * 8192 + 1)), 65);
    assert_eq!(rtfilter_set_max(Some(8_388_608)), 1024);
    assert_eq!(rtfilter_set_max(Some(u64::MAX)), u32::MAX as usize);
}
