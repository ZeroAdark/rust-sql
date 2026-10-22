use std::sync::{Mutex, MutexGuard, Once};

use mcx::MemoryContext;

use crate::checksum::{PgChecksumContext, PgChecksumType};
use crate::*;

const TEST_SYSID: u64 = 1234567890123456789;

static CWD: Mutex<()> = Mutex::new(());

// The manifest lives in a BufFile temp file under the datadir's base/pgsql_tmp:
// stand up fd's temp-file access the way sort_storage's tests do and hold the
// cwd lock for the test's duration.
fn install_seams(tag: &str) -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        seams::get_system_identifier::set(|| TEST_SYSID);
        pgtz_seams::pg_open_tzfile::set(|_name, _canon, _buf| Ok(None));
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        aio_seams::pgaio_closing_fd::set(|_| {});
        aio_seams::pgaio_io_start_readv::set(|_, _, _| Ok(()));
        waitevent_seams::pgstat_report_wait_start::set(|_| {});
        waitevent_seams::pgstat_report_wait_end::set(|| {});
        pgstat_seams::pgstat_report_tempfile::set(|_| {});
        ipc_seams::before_shmem_exit::set(|_, _| Ok(()));
        ipc_seams::on_shmem_exit::set(|_cb, _arg| {});
        resowner::init_seams();
    });
    let guard = CWD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = format!(
        "{}/pgrust-manifest-{}-{tag}",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(format!("{dir}/base/pgsql_tmp")).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    fd::InitFileAccess();
    let _ = fd::InitTemporaryFileAccess();
    if resowner_seams::current_resource_owner::call().is_null() {
        let owner =
            resowner::ResourceOwnerCreate(types_resowner::ResourceOwner::NULL, "manifest-test")
                .unwrap();
        resowner_seams::set_current_resource_owner::call(owner);
    }
    guard
}

// Drain SendBackupManifest through a deliberately small sink buffer so the
// chunked read path is exercised.
fn send_all(m: &mut BackupManifestInfo<'_>, buffer_length: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; buffer_length];
    SendBackupManifest(m, buffer_length, |read_exact, n| {
        read_exact(&mut chunk[..n])?;
        out.extend_from_slice(&chunk[..n]);
        Ok(())
    })
    .unwrap();
    out
}

// SHA-256 of BODY computed by an external tool (Python hashlib): the checksum oracle.
const BODY: &str = concat!(
    "{ \"PostgreSQL-Backup-Manifest-Version\": 2,\n",
    "\"System-Identifier\": 1234567890123456789,\n",
    "\"Files\": [\n",
    "{ \"Path\": \"base/1/1259\", \"Size\": 8192, ",
    "\"Last-Modified\": \"2023-11-14 22:13:20 GMT\" }\n",
    "],\n",
    "\"WAL-Ranges\": [\n",
    "{ \"Timeline\": 1, \"Start-LSN\": \"0/16B3D50\", \"End-LSN\": \"0/16C0000\" }\n",
    "],\n",
);
const BODY_SHA256: &str = "74a12417758631cb0a52648f9d24d5c6439b569874d2937d77a096549133a95f";

#[test]
fn full_manifest_bytes_and_checksum() {
    let _cwd = install_seams("full");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();

    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::None).unwrap();

    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    AddFileToBackupManifest(&mut m, 0, b"base/1/1259", 8192, 1_700_000_000, &mut cc).unwrap();

    AddWALInfoToBackupManifest(mcx, &mut m, 0x016B3D50, 1, 0x016C0000, 1).unwrap();

    let bytes = send_all(&mut m, 100);

    let expected = format!("{BODY}\"Manifest-Checksum\": \"{BODY_SHA256}\"}}\n");
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        expected,
        "manifest bytes drifted from C format"
    );
    assert_eq!(m.manifest_size(), bytes.len() as u64);
}

// backup_manifest.c:68 InitializeBackupManifest: the manifest accumulates in a
// BufFile temp file, so temp_file_limit applies to it (fd.c FileWriteV).
#[test]
fn manifest_counts_against_temp_file_limit() {
    let _cwd = install_seams("limit");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();

    let saved = guc_tables::vars::temp_file_limit.read();
    guc_tables::vars::temp_file_limit.write(1);

    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::None).unwrap();
    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    let mut result = Ok(());
    for i in 0..200 {
        let path = format!("base/1/{i}");
        result = AddFileToBackupManifest(&mut m, 0, path.as_bytes(), 8192, 1_700_000_000, &mut cc);
        if result.is_err() {
            break;
        }
    }
    guc_tables::vars::temp_file_limit.write(saved);

    let err = result.unwrap_err();
    assert_eq!(err.sqlstate(), ::types_error::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED);
    assert_eq!(err.message(), "temporary file size exceeds \"temp_file_limit\" (1kB)");
}

#[test]
fn manifest_within_temp_file_limit_is_sent() {
    let _cwd = install_seams("limitok");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();

    let saved = guc_tables::vars::temp_file_limit.read();
    guc_tables::vars::temp_file_limit.write(64);

    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::None).unwrap();
    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    for i in 0..200 {
        let path = format!("base/1/{i}");
        AddFileToBackupManifest(&mut m, 0, path.as_bytes(), 8192, 1_700_000_000, &mut cc).unwrap();
    }
    AddWALInfoToBackupManifest(mcx, &mut m, 0x016B3D50, 1, 0x016C0000, 1).unwrap();
    let bytes = send_all(&mut m, 8192);
    guc_tables::vars::temp_file_limit.write(saved);

    assert!(bytes.len() > 8192, "{}", bytes.len());
    assert_eq!(m.manifest_size(), bytes.len() as u64);
    assert!(bytes.ends_with(b"\"}\n"));
    assert_eq!(bytes.windows(8).filter(|w| w == b"\"Path\": ").count(), 200);
}

#[test]
fn crc32c_known_vector() {
    let mut cc = PgChecksumContext::init(PgChecksumType::Crc32c);
    cc.update(b"123456789");
    let mut out = [0u8; PG_CHECKSUM_MAX_LENGTH];
    let n = cc.finalize(&mut out);
    assert_eq!(n, 4);
    assert_eq!(out[..4], 0xE306_9283u32.to_le_bytes());
}

#[test]
fn crc32c_is_default_and_renders() {
    let _cwd = install_seams("crc");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();

    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::Crc32c).unwrap();

    let mut cc = PgChecksumContext::init(PgChecksumType::Crc32c);
    cc.update(b"123456789");
    AddFileToBackupManifest(&mut m, 0, b"global/pg_control", 8192, 1_700_000_000, &mut cc).unwrap();
    AddWALInfoToBackupManifest(mcx, &mut m, 0x016B3D50, 1, 0x016C0000, 1).unwrap();
    let bytes = send_all(&mut m, 32768);
    let text = std::str::from_utf8(&bytes).unwrap();

    assert!(
        text.contains("\"Checksum-Algorithm\": \"CRC32C\", \"Checksum\": \"839206e3\""),
        "checksum field wrong: {text}"
    );
}

#[test]
fn tablespace_path_prefix() {
    let _cwd = install_seams("tblspc");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();
    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::None).unwrap();
    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    AddFileToBackupManifest(&mut m, 16400, b"16401/12345", 100, 1_700_000_000, &mut cc).unwrap();
    let bytes = send_all(&mut m, 32768);
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("\"Path\": \"pg_tblspc/16400/16401/12345\""), "{text}");
}

#[test]
fn encoded_path_for_invalid_utf8() {
    let _cwd = install_seams("utf8");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();
    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_YES, PgChecksumType::None).unwrap();
    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    AddFileToBackupManifest(&mut m, 0, b"ab\xFF", 1, 1_700_000_000, &mut cc).unwrap();
    let text = String::from_utf8(send_all(&mut m, 32768)).unwrap();
    assert!(text.contains("\"Encoded-Path\": \"6162ff\""), "{text}");
}

#[test]
fn disabled_manifest_is_empty() {
    let _cwd = install_seams("off");
    let ctx = MemoryContext::new("manifest-test");
    let mcx = ctx.mcx();
    let mut m = BackupManifestInfo::zeroed();
    InitializeBackupManifest(mcx, &mut m, MANIFEST_OPTION_NO, PgChecksumType::None).unwrap();
    let mut cc = PgChecksumContext::init(PgChecksumType::None);
    AddFileToBackupManifest(&mut m, 0, b"base/1/1", 1, 1, &mut cc).unwrap();
    assert!(send_all(&mut m, 32768).is_empty());
    assert_eq!(m.manifest_size(), 0);
}
