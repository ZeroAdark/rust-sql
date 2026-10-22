//! D580: the dense fold's domain is the statement's own zone extrema (`granule_source::zone_extrema_of`), read under D336's
//! bracket (`bracketed`, which the census walk and `zone_minima` share): a read of the latest committed generation's zone
//! facts speaks for the list only when the list's generation is that one both before and after the read. A retiring
//! writer (a merge, a compaction: M5e) publishing between the two resolves makes it no bound, where the unbracketed
//! `zone_extrema` gave the new generation's range and a key outside it raised XX000. A server-level case needs such a
//! writer; until then (main 09-25) the bracket itself.
use gravity_hosts::ops::scan::pgrc::granule_source::bracketed;

/// `bracketed` over the generations the two resolves see, and whether the read ran.
fn run(gens: [Option<u64>; 2]) -> (Option<(i64, i64)>, bool) {
    let (mut i, mut read) = (0, false);
    let r = bracketed(
        &mut || {
            i += 1;
            Ok(gens[i - 1])
        },
        || {
            read = true;
            Ok((3i64, 9i64))
        },
    )
    .expect("no error");
    (r, read)
}

#[test]
fn a_read_speaks_only_when_the_generation_holds_across_it() {
    assert_eq!(run([Some(7), Some(7)]), (Some((3, 9)), true), "the list's generation throughout: its extrema");
    assert_eq!(run([Some(7), Some(8)]), (None, true), "a generation published between the resolves: no bound");
    assert_eq!(run([None, Some(7)]), (None, false), "the list is not the latest generation: nothing read");
    assert_eq!(run([Some(7), None]), (None, true), "the list's parts retired under the read: no bound");
}

#[test]
fn a_read_error_is_returned_not_taken_for_no_bound() {
    let r = bracketed(&mut || Ok(Some(1)), || -> types_error::PgResult<()> { Err(Box::new(types_error::PgError::error("gravity: a zone column read failed"))) });
    assert!(r.is_err());
}
