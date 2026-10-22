use crate::{appendf, rec_data, rec_info, Rec, XLR_INFO_MASK};
use storage_xlog::{XLOG_SMGR_CREATE, XLOG_SMGR_CREATE_TABLE_DIR, XLOG_SMGR_TABLE_DIR_GEN, XLOG_SMGR_TRUNCATE};
use stringinfo::StringInfo;
use types_core::ForkNumber;
use types_error::PgResult;
use types_storage::RelFileLocator;
use xlogreader_seams::XLogReaderState;

pub fn smgr_desc(buf: &mut StringInfo<'_>, record: &XLogReaderState) -> PgResult<()> {
    let rec = Rec(rec_data(record));
    let info = rec_info(record) & !XLR_INFO_MASK;

    if info == XLOG_SMGR_CREATE {
        // xl_smgr_create: rlocator 0..12, forkNum 12.
        let what = "xl_smgr_create";
        let rlocator =
            RelFileLocator::new(rec.u32(0, what)?, rec.u32(4, what)?, rec.u32(8, what)?);
        let forknum = ForkNumber::from_i32(rec.i32(12, what)?)
            .unwrap_or(ForkNumber::MAIN_FORKNUM);
        buf.append_str(&relpath_seams::relpathperm::call(rlocator, forknum))?;
    } else if info == XLOG_SMGR_TRUNCATE {
        // xl_smgr_truncate: blkno 0, rlocator 4..16, flags 16.
        let what = "xl_smgr_truncate";
        let rlocator =
            RelFileLocator::new(rec.u32(4, what)?, rec.u32(8, what)?, rec.u32(12, what)?);
        appendf!(
            buf,
            "{} to {} blocks flags {}",
            relpath_seams::relpathperm::call(rlocator, ForkNumber::MAIN_FORKNUM),
            rec.u32(0, what)?,
            rec.i32(16, what)?
        )?;
    } else if info == XLOG_SMGR_CREATE_TABLE_DIR {
        let what = "xl_smgr_create_table_dir";
        let rlocator =
            RelFileLocator::new(rec.u32(0, what)?, rec.u32(4, what)?, rec.u32(8, what)?);
        let main = relpath_seams::relpathperm::call(rlocator, ForkNumber::MAIN_FORKNUM);
        // trunk's pgrc_format has no table_dir_beside: the same rule as pgrc_am dirpath.rs
        let name = pgrc_format::dirlayout::table_dir_name(rlocator.relNumber as u64);
        let dir = match main.rfind('/') {
            Some(i) => format!("{}/{}", &main[..i], name),
            None => name,
        };
        buf.append_str(&dir)?;
    } else if info == XLOG_SMGR_TABLE_DIR_GEN {
        // D3150: rlocator 0..12, then the stamp's four u64 (pgrc_am::walgen).
        let what = "xl_smgr_table_dir_gen";
        let rlocator =
            RelFileLocator::new(rec.u32(0, what)?, rec.u32(4, what)?, rec.u32(8, what)?);
        let data = rec_data(record);
        let stamp = data.get(12..).and_then(pgrc_am::walgen::WalGen::from_payload);
        buf.append_str(&storage_xlog::table_dir_of(rlocator))?;
        if let Some(w) = stamp {
            appendf!(buf, " gen {} publisher {} base {} base publisher {}", w.gen, w.publisher_fxid, w.base_gen, w.base_publisher_fxid)?;
        }
    }
    Ok(())
}

pub fn smgr_identify(info: u8) -> Option<&'static str> {
    match info & !XLR_INFO_MASK {
        XLOG_SMGR_CREATE => Some("CREATE"),
        XLOG_SMGR_TRUNCATE => Some("TRUNCATE"),
        XLOG_SMGR_CREATE_TABLE_DIR => Some("CREATE_TABLE_DIR"),
        XLOG_SMGR_TABLE_DIR_GEN => Some("TABLE_DIR_GEN"),
        _ => None,
    }
}
