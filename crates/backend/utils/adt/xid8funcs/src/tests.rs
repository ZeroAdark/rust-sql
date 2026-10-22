use mcx::MemoryContext;

use crate::*;

fn view<'a>(v: &'a ::datum::Varlena<'_>) -> SnapView<'a> {
    SnapView::new(v.data())
}

fn parse<'m>(mcx: ::mcx::Mcx<'m>, s: &str) -> PgResult<Option<::datum::Varlena<'m>>> {
    parse_snapshot(mcx, s.as_bytes(), None)
}

#[test]
fn strtou64_libc_semantics() {
    assert_eq!(strtou64(b"12:13:"), (12, 2));
    assert_eq!(strtou64(b"  42"), (42, 4));
    assert_eq!(strtou64(b"+7x"), (7, 2));
    assert_eq!(strtou64(b"-1"), (u64::MAX, 2));
    assert_eq!(strtou64(b"18446744073709551616"), (u64::MAX, 20));
    assert_eq!(strtou64(b"x1"), (0, 0));
    assert_eq!(strtou64(b""), (0, 0));
    assert_eq!(strtou64(b"18446744073709551615"), (u64::MAX, 20));
    assert_eq!(strtou64(b"-18446744073709551616"), (u64::MAX, 21));
    assert_eq!(strtou64(b"-518446744073709551616"), (u64::MAX, 22));
}

#[test]
fn parse_out_round_trip() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    for s in [
        "12:13:",
        "12:18:14,16",
        "12:16:14,14",
        "31:31:",
        "8589934593:8589934593:",
        "8589934593:8589934595:8589934594",
    ] {
        let v = parse(mcx, s).unwrap().unwrap();
        let out = snapshot_out_bytes(mcx, &view(&v)).unwrap();
        let expect = if s == "12:16:14,14" { "12:16:14" } else { s };
        assert_eq!(core::str::from_utf8(&out).unwrap(), expect, "input {s}");
    }
}

#[test]
fn out_reservation_never_exceeds_max_alloc() {
    assert!(out_reservation(52_000_000) <= MAX_ALLOC_SIZE);
    assert!(out_reservation(PG_SNAPSHOT_MAX_NXIP) <= MAX_ALLOC_SIZE);
    assert_eq!(out_reservation(0), 44);
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let xips: Vec<u64> = (100..300_100).collect();
    let img = snapshot_image(mcx, 100, 1_000_000, &xips).unwrap();
    let out = snapshot_out_bytes(mcx, &view(&img)).unwrap();
    assert!(out.len() > out_reservation(xips.len()));
    let text = core::str::from_utf8(&out).unwrap();
    assert!(text.starts_with("100:1000000:100,101,102,"));
    assert!(text.ends_with(",300098,300099"));
}

#[test]
fn parse_errors_22p02() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    for s in [
        "31:12:",
        "0:1:",
        "12:13:0",
        "12:16:14,13",
        "12:16:14,,16",
        "12:16:14 16",
        "12",
        "12:",
        "12:13",
        ":",
        "",
        "12:13:14",
    ] {
        let err = parse(mcx, s).unwrap_err();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_INVALID_TEXT_REPRESENTATION, "input {s}");
        assert!(
            err.message().contains("invalid input syntax for type pg_snapshot"),
            "input {s}: {}",
            err.message()
        );
    }
}

#[test]
fn parse_soft_error_returns_none() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mut esc = types_error::SoftErrorContext::new(false);
    let r = parse_snapshot(mcx, b"31:12:", Some(&mut esc)).unwrap();
    assert!(r.is_none());
    assert!(esc.error_occurred());
}

#[test]
fn image_layout_matches_c() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let v = parse(mcx, "12:18:14,16").unwrap().unwrap();
    assert_eq!(v.varsize(), 24 + 16);
    let s = view(&v);
    assert_eq!(s.nxip(), 2);
    assert_eq!(s.xmin(), 12);
    assert_eq!(s.xmax(), 18);
    assert_eq!(s.xip(0), 14);
    assert_eq!(s.xip(1), 16);
}

#[test]
fn visible_fxid_linear_and_bsearch() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let v = parse(mcx, "10:100:20,30,40").unwrap().unwrap();
    let s = view(&v);
    assert!(is_visible_fxid(9, &s));
    assert!(is_visible_fxid(10, &s));
    assert!(!is_visible_fxid(100, &s));
    assert!(!is_visible_fxid(200, &s));
    assert!(!is_visible_fxid(30, &s));
    assert!(is_visible_fxid(31, &s));

    let xips: Vec<u64> = (100..164).step_by(2).collect();
    let img = snapshot_image(mcx, 100, 1000, &xips).unwrap();
    let s = view(&img);
    assert_eq!(s.nxip() as usize, xips.len());
    assert!(s.nxip() > USE_BSEARCH_IF_NXIP_GREATER);
    for &x in &xips {
        assert!(!is_visible_fxid(x, &s));
        assert!(is_visible_fxid(x + 1, &s));
    }
    assert!(is_visible_fxid(99, &s));
    assert!(is_visible_fxid(999, &s));
    assert!(!is_visible_fxid(1000, &s));
}

#[test]
fn recv_vectors() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mk = |nxip: i32, xmin: u64, xmax: u64, xips: &[u64]| {
        let mut b = Vec::new();
        b.extend_from_slice(&nxip.to_be_bytes());
        b.extend_from_slice(&xmin.to_be_bytes());
        b.extend_from_slice(&xmax.to_be_bytes());
        for &x in xips {
            b.extend_from_slice(&x.to_be_bytes());
        }
        b
    };

    let si_of = |wire: &[u8]| {
        let mut si = stringinfo::StringInfo::new_in(mcx).unwrap();
        si.append_bytes(wire).unwrap();
        si
    };

    let mut si = si_of(&mk(2, 12, 18, &[14, 16]));
    let v = snapshot_recv(mcx, &mut si).unwrap();
    let s = view(&v);
    assert_eq!((s.nxip(), s.xmin(), s.xmax()), (2, 12, 18));

    let mut si = si_of(&mk(3, 12, 18, &[14, 14, 16]));
    let v = snapshot_recv(mcx, &mut si).unwrap();
    let s = view(&v);
    assert_eq!(s.nxip(), 2);
    assert_eq!((s.xip(0), s.xip(1)), (14, 16));

    for bad in [
        mk(-1, 12, 18, &[]),
        mk(0, 18, 12, &[]),
        mk(0, 0, 12, &[]),
        mk(1, 12, 18, &[11]),
        mk(1, 12, 18, &[19]),
        mk(2, 12, 18, &[16, 14]),
    ] {
        let mut si = si_of(&bad);
        let err = snapshot_recv(mcx, &mut si).unwrap_err();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_INVALID_BINARY_REPRESENTATION);
    }
}

#[test]
fn allowable_at_epochs() {
    assert_eq!(full_xid_from_allowable_at((3 << 32) | 100, 50), (3 << 32) | 50);
    assert_eq!(full_xid_from_allowable_at((3 << 32) | 100, 200), (2 << 32) | 200);
    assert_eq!(full_xid_from_allowable_at((3 << 32) | 100, 2), 2);
    assert_eq!(full_xid_from_allowable_at((3 << 32) | 100, 0), 0);
}

#[test]
fn bad_format_keeps_non_utf8_input_bytes() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let e = parse_snapshot(mcx, b"\xff", None).unwrap_err();
    assert_eq!(
        e.message_raw.as_deref(),
        Some(&b"invalid input syntax for type pg_snapshot: \"\xff\""[..])
    );
    let e = parse_snapshot(mcx, b"10:20:\xff", None).unwrap_err();
    assert_eq!(
        e.message_raw.as_deref(),
        Some(&b"invalid input syntax for type pg_snapshot: \"10:20:\xff\""[..])
    );
    let e = parse(mcx, "10:20:x").unwrap_err();
    assert_eq!(e.message_raw, None);
    assert_eq!(e.message(), "invalid input syntax for type pg_snapshot: \"10:20:x\"");
}
