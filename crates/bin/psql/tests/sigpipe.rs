//! SIGPIPE disposition matches C psql: with stdout a pipe whose reader has
//! gone, psql is killed by SIGPIPE on its first write (C psql keeps SIGPIPE at
//! SIG_DFL outside `\o |cmd` and the pager), instead of the Rust runtime's
//! SIG_IGN turning it into EPIPE, after which the port kept executing the rest
//! of a script and then panicked in println!. The end-to-end form, with a
//! server (`psql -f script.sql | head -1`: the later statement must not run),
//! is gate/sigpipe-gate.sh.

#![cfg(unix)]

use std::os::unix::io::FromRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

#[test]
fn closed_stdout_pipe_kills_psql_with_sigpipe() {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid 2-element array for pipe(2).
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // The reader is gone before psql starts: its first write gets SIGPIPE.
    // SAFETY: fds[0] is the read end just created and owned here.
    unsafe { libc::close(fds[0]) };
    // SAFETY: fds[1] is the write end just created; ownership moves to Stdio.
    let out = unsafe { Stdio::from_raw_fd(fds[1]) };
    let status = Command::new(env!("CARGO_BIN_EXE_psql"))
        .arg("--help")
        .stdout(out)
        .stderr(Stdio::piped())
        .output()
        .expect("spawn psql")
        .status;
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "psql must die by SIGPIPE like C psql, got {status:?}"
    );
}
