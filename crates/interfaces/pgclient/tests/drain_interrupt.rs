// dblink.c materializeQueryResult PG_CATCH: the libpqsrv_get_result drain
// loop services interrupts, so a statement_timeout firing while the remote
// is still busy replaces the error being cleaned up after (wio P227).
// Own binary: the seams are process-global.
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);

fn setup_backend() -> types_storage::latch::LatchHandle {
    waitevent_seams::pgstat_report_wait_start::set(|_| {});
    waitevent_seams::pgstat_report_wait_end::set(|| {});
    postgres_seams::check_for_interrupts::set(|| {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(Box::new(
                types_error::PgError::error("canceling statement due to statement timeout")
                    .with_sqlstate(types_error::ERRCODE_QUERY_CANCELED),
            ));
        }
        Ok(())
    });
    init_small::init_seams();
    waiteventset::init_seams();
    latch::init_seams();
    init_small::globals::SetMyProcPid(std::process::id() as i32);
    fd::vfd::set_max_safe_fds_value(1000);
    waiteventset::InitializeWaitEventSupport().unwrap();
    let l = latch::allocate_local_latch();
    latch::InitLatch(l);
    init_small::globals::SetMyLatch(Some(l));
    l
}

// Accepts one client, completes a trust startup, then swallows every query
// without answering (a remote still running pg_sleep).
fn silent_server() -> (u16, std::thread::JoinHandle<()>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut len = [0u8; 4];
        s.read_exact(&mut len).unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize - 4];
        s.read_exact(&mut body).unwrap();
        let mut out = vec![b'R'];
        out.extend_from_slice(&8i32.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        out.push(b'Z');
        out.extend_from_slice(&5i32.to_be_bytes());
        out.push(b'I');
        s.write_all(&out).unwrap();
        let mut sink = [0u8; 1024];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    });
    (port, h)
}

#[test]
fn drain_propagates_interrupt_while_remote_busy() {
    let latch = setup_backend();
    let (port, _srv) = silent_server();
    let we = pgclient::WaitEvents { connect: 0, receive: 0 };
    let conninfo = format!("hostaddr=127.0.0.1 port={port} user=u sslmode=disable gssencmode=disable");
    let mut conn = pgclient::connect_db(&conninfo, None, we).unwrap().unwrap();
    assert!(conn.send_query("select pg_sleep(2)"));
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        CANCEL.store(true, Ordering::SeqCst);
        latch::SetLatch(latch);
    });
    let err = conn.drain_interruptible().err().expect("drain must surface the interrupt");
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    assert!(conn.is_busy());
}
