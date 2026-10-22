//! Server-start priming's directory walk (D238: through the VFS, not std::fs): `columnar_dirs` finds every
//! `pgrc_<n>` table directory under base/ and under a tablespace link, keyed by its locator, sorted by path.
//! One test in its own binary: the walk is relative to the working directory (the data directory in a server).
use gravity_hosts::ops::scan::pgrc::prime::columnar_dirs;

#[test]
fn columnar_dirs_finds_base_and_tablespace_tables_in_path_order() {
    let root = std::env::temp_dir().join(format!("gravity-prime-dirs-{}", std::process::id()));
    let spc = std::env::temp_dir().join(format!("gravity-prime-spc-{}", std::process::id()));
    for d in ["base/5/pgrc_20000", "base/5/pgrc_x", "base/5/16384", "base/1/pgrc_16390", "base/pgsql_tmp", "pg_tblspc"] {
        std::fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    std::fs::create_dir_all(spc.join("PG_18_202406281/5/pgrc_30000")).expect("mkdir");
    std::os::unix::fs::symlink(&spc, root.join("pg_tblspc/17000")).expect("tablespace link");
    let before = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("chdir");
    let got = columnar_dirs();
    std::env::set_current_dir(before).expect("chdir back");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&spc);
    let got: Vec<(u32, u32, u32, String)> = got.into_iter().map(|(l, p)| (l.spcOid, l.dbOid, l.relNumber, p)).collect();
    assert_eq!(
        got,
        vec![
            (1663, 1, 16390, "base/1/pgrc_16390".to_string()),
            (1663, 5, 20000, "base/5/pgrc_20000".to_string()),
            (17000, 5, 30000, "pg_tblspc/17000/PG_18_202406281/5/pgrc_30000".to_string()),
        ]
    );
}
