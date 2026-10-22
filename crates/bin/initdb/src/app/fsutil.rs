//! Frontend file helpers (src/common/file_utils.c, pgcheckdir.c, pgmkdirp.c).

use std::path::Path;

/// `pg_check_dir`: 0 not there, 1 empty, 2 only lost+found/dotfiles, 3 mount
/// point, 4 non-empty, -1 error.
pub fn pg_check_dir(dir: &Path) -> i32 {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(_) => -1,
        Ok(entries) => {
            let mut result = 1;
            let mut dot_found = false;
            let mut mount_found = false;
            for e in entries.flatten() {
                let name = e.file_name();
                let n = name.to_string_lossy();
                if n.starts_with('.') && n != "." && n != ".." {
                    dot_found = true;
                } else if n == "lost+found" {
                    mount_found = true;
                } else {
                    return 4;
                }
            }
            if mount_found { result = 3; } else if dot_found { result = 2; }
            result
        }
    }
}

pub fn mkdir_p(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(mode).create(path)
}

pub fn mkdir(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(mode).create(path)
}

pub fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn fsync_fname(path: &Path, isdir: bool) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
    let flags = if isdir { libc::O_RDONLY } else { libc::O_RDWR };
    // SAFETY: NUL-terminated path; fd owned here.
    unsafe {
        let fd = libc::open(c.as_ptr(), flags);
        if fd < 0 {
            return;
        }
        if libc::fsync(fd) != 0 {
            eprintln!("initdb: warning: could not fsync file \"{}\": {}", path.display(), std::io::Error::last_os_error());
        }
        libc::close(fd);
    }
}

/// `fsync_dir_recurse`.
pub fn fsync_dir_recurse(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => fsync_dir_recurse(&p),
                Ok(t) if t.is_file() => fsync_fname(&p, false),
                _ => {}
            }
        }
    }
    fsync_fname(dir, true);
}

/// `sync_pgdata` (fsync method): parent dir, then everything under it.
pub fn sync_pgdata(pg_data: &Path) {
    if let Some(parent) = pg_data.parent() {
        fsync_fname(parent, true);
    }
    fsync_dir_recurse(pg_data);
}
