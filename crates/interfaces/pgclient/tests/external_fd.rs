// libpq-be-fe-helpers.h:131-154 / :107-121: one external FD is reserved
// before the dial and released with the connection (here: on the failed
// dial); a refused reservation is the 08001 error with C's DETAIL/HINT.
// Own binary: the seams are process-global and the lib tests dial too.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ACQUIRES: AtomicUsize = AtomicUsize::new(0);
static RELEASES: AtomicUsize = AtomicUsize::new(0);
static ADMIT: AtomicBool = AtomicBool::new(true);

#[test]
fn connect_reserves_and_releases_one_external_fd() {
    file_seams::acquire_external_fd::set(|| {
        ACQUIRES.fetch_add(1, Ordering::Relaxed);
        Ok(ADMIT.load(Ordering::Relaxed))
    });
    file_seams::release_external_fd::set(|| {
        RELEASES.fetch_add(1, Ordering::Relaxed);
    });
    let p = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let we = pgclient::WaitEvents { connect: 0, receive: 0 };
    let conninfo = format!("hostaddr=127.0.0.1 port={p} user=u");
    let err = pgclient::connect_db(&conninfo, None, we).unwrap().err().unwrap();
    assert!(err.starts_with(&format!("connection to server at \"127.0.0.1\", port {p} failed: ")), "{err}");
    assert_eq!((ACQUIRES.load(Ordering::Relaxed), RELEASES.load(Ordering::Relaxed)), (1, 1));

    ADMIT.store(false, Ordering::Relaxed);
    let Err(err) = pgclient::connect_db(&conninfo, None, we) else { panic!("reservation refused, yet dialed") };
    assert_eq!(err.message, "could not establish connection");
    assert_eq!(err.sqlstate, types_error::ERRCODE_SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION);
    assert_eq!(err.detail.as_deref(), Some("There are too many open files on the local server."));
    assert_eq!(
        err.hint.as_deref(),
        Some("Raise the server's \"max_files_per_process\" and/or \"ulimit -n\" limits.")
    );
    assert_eq!((ACQUIRES.load(Ordering::Relaxed), RELEASES.load(Ordering::Relaxed)), (2, 1));
}
