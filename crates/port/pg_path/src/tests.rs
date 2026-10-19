use super::*;

#[test]
fn canonicalize_pins_c_behavior() {
    for (input, want) in [
        ("", ""),
        ("/", "/"),
        ("/a//b", "/a/b"),
        ("/a///b/", "/a/b"),
        ("/a/./b", "/a/b"),
        ("/a/b/..", "/a"),
        ("/a/b/../..", "/"),
        ("/a/b/../../..", "/"),
        ("/..", "/"),
        ("/../..", "/"),
        (".", "."),
        ("./", "."),
        ("..", ".."),
        ("../..", "../.."),
        ("a/..", "."),
        ("../dir/..", ".."),
        ("../dir/../x", "../x"),
        ("a/b/../c", "a/c"),
        ("a/./b", "a/b"),
        ("foo/bar/", "foo/bar"),
        ("/a/../b", "/b"),
        ("../a/b/../../c", "../c"),
    ] {
        assert_eq!(canonicalize_path(input), want, "input {input:?}");
    }
}

#[test]
fn join_omits_slash_for_empty_components() {
    assert_eq!(join_path_components("a", "b"), "a/b");
    assert_eq!(join_path_components("", "b"), "b");
    assert_eq!(join_path_components("a", ""), "a");
    assert_eq!(join_path_components("/", "b"), "//b");
}

#[test]
fn join_is_bounded_to_maxpgpath() {
    // path.c:300: strlcpy + snprintf into MAXPGPATH; a 1020-byte head
    // leaves room for "/" and two bytes of the tail.
    let head = "./".repeat(510);
    let joined = join_path_components(&head, "postgres");
    assert_eq!(joined.len(), MAXPGPATH - 1);
    assert_eq!(joined, format!("{head}/po"));
    assert_eq!(
        join_path_components_bytes(head.as_bytes(), b"postgres"),
        joined.as_bytes()
    );
    let long_head = "a".repeat(MAXPGPATH + 5);
    assert_eq!(join_path_components(&long_head, "x").len(), MAXPGPATH - 1);
    assert_eq!(join_path_components_bytes(long_head.as_bytes(), b"").len(), MAXPGPATH - 1);
    // C's join then canonicalize turns the repeated-prefix candidate into
    // "po", never "postgres" (regression shape).
    assert_eq!(canonicalize_path(&joined), "po");
}

#[test]
fn get_parent_directory_matches_trim_directory() {
    assert_eq!(get_parent_directory("/a/b/c"), "/a/b");
    assert_eq!(get_parent_directory("/a"), "/");
    assert_eq!(get_parent_directory("a"), "");
    assert_eq!(get_parent_directory("/a/b//"), "/a");
    assert_eq!(get_parent_directory("/"), "/");
}

#[test]
fn make_relative_path_relocates_per_c_comment() {
    // The worked example in path.c's make_relative_path header.
    assert_eq!(
        make_relative_path(
            "/usr/local/share/postgresql",
            "/usr/local/bin",
            "/opt/pgsql/bin/postgres"
        ),
        "/opt/pgsql/share/postgresql"
    );
    assert_eq!(
        make_relative_path(
            "/usr/local/pgsql/share",
            "/usr/local/pgsql/bin",
            "/home/me/inst/bin/postgres"
        ),
        "/home/me/inst/share"
    );
    // No tail match: compiled-in target wins.
    assert_eq!(
        make_relative_path(
            "/usr/local/pgsql/share",
            "/usr/local/pgsql/bin",
            "/somewhere/else/postgres"
        ),
        "/usr/local/pgsql/share"
    );
    // No common prefix.
    assert_eq!(
        make_relative_path("/opt/share", "/usr/bin", "/x/bin/postgres"),
        "/opt/share"
    );
    // '/usr/lib' vs '/usr/libexec': prefix must end on a separator.
    assert_eq!(
        make_relative_path("/usr/lib", "/usr/libexec", "/inst/libexec/postgres"),
        "/inst/lib"
    );
}

#[test]
fn share_path_is_absolute() {
    let share = get_share_path("/nonexistent/bin/postgres");
    assert!(is_absolute_path(&share));
}

#[test]
fn validate_and_find_my_exec() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("pg_path_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("fakepg");
    std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exe_str = exe.to_str().unwrap();

    assert_eq!(validate_exec(exe_str), 0);
    // exec.c:124: a non-regular file sets errno itself (EISDIR for a
    // directory) so the caller's %m names the real reason.
    test_set_errno(0);
    assert_eq!(validate_exec(dir.to_str().unwrap()), -1);
    assert_eq!(test_errno(), libc::EISDIR);
    assert_eq!(validate_exec("/no/such/file"), -1);
    assert_eq!(test_errno(), libc::ENOENT);

    let mut logged = Vec::new();
    let found = find_my_exec(exe_str, |_, m| logged.push(m)).unwrap();
    assert!(is_absolute_path(&found));
    assert!(found.ends_with("/fakepg"));
    assert!(logged.is_empty());

    // exec.c:173: separator present + validate_exec failure = one LOG line
    // 'invalid binary "%s": %m' (ERRCODE_WRONG_OBJECT_TYPE) before -1.
    let mut logged = Vec::new();
    find_my_exec("/no/such/file", |c, m| logged.push((c, m))).unwrap_err();
    assert_eq!(
        logged,
        vec![(
            ExecLogCode::WrongObjectType,
            "invalid binary \"/no/such/file\": No such file or directory".to_string()
        )]
    );
    let mut logged = Vec::new();
    find_my_exec(dir.to_str().unwrap(), |c, m| logged.push((c, m))).unwrap_err();
    assert_eq!(
        logged,
        vec![(
            ExecLogCode::WrongObjectType,
            format!("invalid binary \"{}\": Is a directory", dir.to_str().unwrap())
        )]
    );
    assert_eq!(ExecLogCode::WrongObjectType.sqlstate(), Some(*b"42809"));
    assert_eq!(ExecLogCode::UndefinedFile.sqlstate(), Some(*b"58P01"));
    assert_eq!(ExecLogCode::FileAccess.sqlstate(), None);

    // PATH-search leg, including the -2 log-and-keep-scanning arm.
    let dir2 = dir.join("unreadable");
    std::fs::create_dir_all(&dir2).unwrap();
    let bad = dir2.join("fakepg");
    std::fs::write(&bad, b"").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o111)).unwrap();

    let saved = std::env::var_os("PATH");
    // exec.c:188 getenv("PATH") is raw bytes: a non-UTF-8 component ahead
    // of the real one is searched (and missed) rather than aborting the scan.
    let mut raw_path = b"/tmp/\xff:".to_vec();
    raw_path.extend_from_slice(format!("{}:{}", dir2.to_str().unwrap(), dir.to_str().unwrap()).as_bytes());
    let raw_path: std::ffi::OsString = std::os::unix::ffi::OsStringExt::from_vec(raw_path);
    std::env::set_var("PATH", raw_path);
    let mut logged = Vec::new();
    let found = find_my_exec("fakepg", |_, m| logged.push(m));
    // exec.c:224: PATH exhausted without a candidate = one LOG line
    // 'could not find a "%s" to execute' (ERRCODE_UNDEFINED_FILE).
    let mut miss_logged = Vec::new();
    let miss = find_my_exec("fakepg-absent", |c, m| miss_logged.push((c, m)));
    // exec.c:167 strlcpy(retpath, argv0, MAXPGPATH) truncates at the byte;
    // an argv0 whose 1023rd byte splits a character must not panic.
    let long_argv0 = format!("{}\u{e9}", "a".repeat(MAXPGPATH - 2));
    let mut long_logged = Vec::new();
    let long_miss = find_my_exec(&long_argv0, |c, m| long_logged.push((c, m)));
    let long_slash = format!("/tmp/{}\u{e9}", "a".repeat(MAXPGPATH - 7));
    let mut slash_logged = Vec::new();
    let long_invalid = find_my_exec(&long_slash, |c, m| slash_logged.push((c, m)));
    match saved {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    long_miss.unwrap_err();
    assert_eq!(
        long_logged,
        vec![(
            ExecLogCode::UndefinedFile,
            format!("could not find a \"{long_argv0}\" to execute")
        )]
    );
    long_invalid.unwrap_err();
    assert_eq!(slash_logged.len(), 1);
    assert_eq!(slash_logged[0].0, ExecLogCode::WrongObjectType);
    assert!(slash_logged[0]
        .1
        .starts_with(&format!("invalid binary \"{}\": ", &long_slash[..MAXPGPATH - 2])));
    miss.unwrap_err();
    assert_eq!(
        miss_logged,
        vec![(
            ExecLogCode::UndefinedFile,
            "could not find a \"fakepg-absent\" to execute".to_string()
        )]
    );
    let found = found.unwrap();
    assert!(found.ends_with("/fakepg"));
    // Skipped when running as root (access(R_OK) succeeds regardless, so the
    // unreadable candidate wins the PATH scan).
    if unsafe { libc::geteuid() } != 0 {
        assert!(!found.contains("unreadable"));
        assert_eq!(logged.len(), 1);
        assert!(logged[0].starts_with("could not read binary"));
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn path_membership_predicates() {
    assert!(path_contains_parent_reference(".."));
    assert!(path_contains_parent_reference("../x"));
    assert!(!path_contains_parent_reference("a/../x"));
    assert!(!path_contains_parent_reference("..x"));
    assert!(path_is_relative_and_below_cwd("base"));
    assert!(!path_is_relative_and_below_cwd("/abs"));
    assert!(!path_is_relative_and_below_cwd("../up"));
    assert!(path_is_prefix_of_path("/data", "/data"));
    assert!(path_is_prefix_of_path("/data", "/data/base"));
    assert!(!path_is_prefix_of_path("/data", "/database"));
}

#[test]
fn separator_scans() {
    assert_eq!(first_dir_separator("a/b"), Some(1));
    assert_eq!(first_dir_separator("ab"), None);
    assert_eq!(last_dir_separator("a/b/c"), Some(3));
    assert_eq!(first_path_var_separator("a:b"), Some(1));
}

fn test_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn test_set_errno(value: i32) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    // SAFETY: writing the calling thread's errno slot.
    unsafe {
        *libc::__error() = value;
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    // SAFETY: writing the calling thread's errno slot.
    unsafe {
        *libc::__errno_location() = value;
    }
}

#[test]
fn normalize_exec_path_reports_realpath_failure() {
    // exec.c:251: realpath failure = one LOG line 'could not resolve path
    // "%s" to absolute form: %m' (errcode_for_file_access) before -1.
    // realpath("") fails with ENOENT on every supported libc.
    let mut logged = Vec::new();
    let err = normalize_exec_path(b"", &mut |c, m| logged.push((c, m, test_errno()))).unwrap_err();
    assert_eq!(
        err,
        "could not resolve path \"\" to absolute form: No such file or directory"
    );
    // errno is ENOENT when the LOG sink runs (errcode_for_file_access reads it).
    assert_eq!(logged, vec![(ExecLogCode::FileAccess, err, libc::ENOENT)]);
}

#[test]
fn path_is_safe_for_extraction_pins_c() {
    // path.c:637: canonicalize, then relative-and-below-cwd.
    assert!(path_is_safe_for_extraction("base/1/2"));
    assert!(path_is_safe_for_extraction("./base/./1"));
    assert!(path_is_safe_for_extraction("base/../pg_wal/x"));
    assert!(!path_is_safe_for_extraction("/etc/passwd"));
    assert!(!path_is_safe_for_extraction("../x"));
    assert!(!path_is_safe_for_extraction("base/../../x"));
    assert!(!path_is_safe_for_extraction(".."));
    let long = "a/".repeat(MAXPGPATH);
    assert!(path_is_safe_for_extraction(&long));
    // strlcpy(buf, path, MAXPGPATH) keeps the first byte of a character
    // split at byte 1023, so the last component is "..\xC3", not "..".
    let split = format!("{}..\u{e9}", "./".repeat(510));
    assert_eq!(split.len(), MAXPGPATH);
    assert!(path_is_safe_for_extraction(&split));
    assert!(!path_is_safe_for_extraction(&split[..MAXPGPATH - 2]));
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn pipe_first_line_discards_partial_line_on_stream_error() {
    // pg_get_line.c:170: data collected before a stream error is dropped,
    // so exec.c:371 reports 'could not read from command "%s": %m'.
    // A non-blocking pipe holding "ab" yields the bytes, then EAGAIN.
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds is a two-element out array; the fds are closed below.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: the write end is open; the buffer outlives the call.
    assert_eq!(unsafe { libc::write(fds[1], b"ab".as_ptr().cast(), 2) }, 2);
    // SAFETY: fcntl on an open descriptor with an integer flag argument.
    assert_eq!(unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) }, 0);
    // SAFETY: fds[0] is open; the stream owns it and is fclose'd below.
    let stream = unsafe { libc::fdopen(fds[0], c"r".as_ptr()) };
    assert!(!stream.is_null());
    let mut logged = Vec::new();
    let line = pipe_first_line(stream, "cmd", &mut |c, m| logged.push((c, m)));
    // SAFETY: stream and fds[1] are open exactly once each.
    unsafe {
        libc::fclose(stream);
        libc::close(fds[1]);
    }
    assert_eq!(line, None);
    assert_eq!(
        logged,
        vec![(
            ExecLogCode::FileAccess,
            format!("could not read from command \"cmd\": {}", strerror(libc::EAGAIN))
        )]
    );

    // The unchanged legs: a complete line, and no data at all.
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: as above.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: as above.
    assert_eq!(unsafe { libc::write(fds[1], b"x\ny".as_ptr().cast(), 3) }, 3);
    // SAFETY: as above.
    unsafe { libc::close(fds[1]) };
    // SAFETY: as above.
    let stream = unsafe { libc::fdopen(fds[0], c"r".as_ptr()) };
    let mut logged = Vec::new();
    let line = pipe_first_line(stream, "cmd", &mut |c, m| logged.push((c, m)));
    assert_eq!(line.as_deref(), Some("x\n"));
    assert!(logged.is_empty());
    let line = pipe_first_line(stream, "cmd", &mut |c, m| logged.push((c, m)));
    assert_eq!(line.as_deref(), Some("y"));
    let line = pipe_first_line(stream, "cmd", &mut |c, m| logged.push((c, m)));
    // SAFETY: stream is open exactly once.
    unsafe { libc::fclose(stream) };
    assert_eq!(line, None);
    assert_eq!(
        logged,
        vec![(ExecLogCode::NoData, "no data was returned by command \"cmd\"".to_string())]
    );
}

#[test]
fn get_home_path_prefers_home_then_passwd() {
    // path.c:1022. HOME is read through the environment, so this test owns
    // the variable for its duration (no other test in this crate touches it).
    let saved = std::env::var_os("HOME");
    std::env::set_var("HOME", "/tmp/pg_path_home");
    let from_env = get_home_path();
    std::env::set_var("HOME", "");
    let from_pw = get_home_path();
    match saved {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    assert_eq!(from_env.as_deref(), Some("/tmp/pg_path_home"));
    // Empty $HOME falls through to getpwuid_r(geteuid()): the passwd entry
    // of the running user exists on any box that can run cargo test.
    let pw = from_pw.expect("passwd entry for the effective uid");
    assert!(!pw.is_empty());
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn find_other_exec_runs_target_dash_v() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("pg_path_other_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mkexe = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let me = mkexe("fakepg", "#!/bin/sh\n");
    mkexe("other", "#!/bin/sh\necho \"other (Fake) 1.0\"\n");
    mkexe("silent", "#!/bin/sh\nexit 3\n");
    let me = me.to_str().unwrap();

    // exec.c:310 happy path: the resolved sibling path, version line
    // compared newline-included (pg_get_line keeps it).
    let mut logged = Vec::new();
    let found = find_other_exec(me, "other", "other (Fake) 1.0\n", |c, m| logged.push((c, m)));
    // find_my_exec realpath()s, so compare against the canonical directory.
    let real_dir = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(found.as_deref(), Ok(real_dir.join("other").to_str().unwrap()));
    assert!(logged.is_empty());

    // -2: ran, wrong version string.
    let found = find_other_exec(me, "other", "other (Fake) 2.0\n", |_, _| {});
    assert_eq!(found, Err(FindOtherExecError::WrongVersion));

    // -1: no such sibling (validate_exec fails, nothing logged).
    let mut logged = Vec::new();
    let found = find_other_exec(me, "missing", "x", |c, m| logged.push((c, m)));
    assert_eq!(found, Err(FindOtherExecError::NotFound));
    assert!(logged.is_empty());

    // -1 via pipe_read_line: no output (ERRCODE_NO_DATA) then pclose_check
    // reports the exit status (ERRCODE_SYSTEM_ERROR, wait_result_to_str).
    let mut logged = Vec::new();
    let found = find_other_exec(me, "silent", "x", |c, m| logged.push((c, m)));
    assert_eq!(found, Err(FindOtherExecError::NotFound));
    let cmd = format!("\"{}\" -V", real_dir.join("silent").to_str().unwrap());
    assert_eq!(
        logged,
        vec![
            (ExecLogCode::NoData, format!("no data was returned by command \"{cmd}\"")),
            (ExecLogCode::SystemError, "child process exited with exit code 3".to_string()),
        ]
    );

    // -1 from find_my_exec itself (its own LOG line).
    let mut logged = Vec::new();
    let found = find_other_exec("/no/such/pg", "other", "x", |c, m| logged.push((c, m)));
    assert_eq!(found, Err(FindOtherExecError::NotFound));
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].0, ExecLogCode::WrongObjectType);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn progname_is_the_last_path_component() {
    assert_eq!(get_progname("/usr/local/pgsql/bin/psql"), "psql");
    assert_eq!(get_progname("bin/mypsql"), "mypsql");
    assert_eq!(get_progname("psql"), "psql");
    assert_eq!(get_progname(""), "");
}

// configure:971-973: docdir = ${datarootdir}/doc/${PACKAGE_TARNAME},
// htmldir = ${docdir} — the "postgresql" component is part of both defaults.
#[test]
fn doc_dirs_default_to_the_configure_layout() {
    let exe = "/usr/local/pgsql/bin/postgres";
    if option_env!("PGRUST_DOCDIR").is_none() {
        assert_eq!(get_doc_path(exe), "/usr/local/pgsql/share/doc/postgresql");
    }
    if option_env!("PGRUST_HTMLDIR").is_none() {
        assert_eq!(get_html_path(exe), "/usr/local/pgsql/share/doc/postgresql");
    }
}
