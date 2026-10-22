use crate::{appendf, rec_data, rec_info, XLR_INFO_MASK};
use pgrc_format::wal::{
    decode_dir, identify, BulkRowsRec, DirLoc, FileRefRec, FileWriteRec, InstallRec, PublishRec, ReclaimRec, Retire,
    RowRec, RowidExtentsRec, RowidReserveRec, TupleImage, XLOG_PGRC_BULK_ROWS, XLOG_PGRC_DIR_CREATE, XLOG_PGRC_FILE_CREATE, XLOG_PGRC_FILE_REF,
    XLOG_PGRC_FILE_WRITE, XLOG_PGRC_INSTALL, XLOG_PGRC_PUBLISH, XLOG_PGRC_RECLAIM, XLOG_PGRC_ROW_DELETE,
    XLOG_PGRC_ROWID_EXTENTS, XLOG_PGRC_ROWID_RESERVE, XLOG_PGRC_ROWID_SET, XLOG_PGRC_ROW_UPDATE, XLOG_PGRC_TRUNCATE,
};
use pgrc_format::FormatError;
use stringinfo::StringInfo;
use types_error::{PgError, PgResult};
use xlogreader_seams::XLogReaderState;

fn bad(e: FormatError) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrc WAL record: {e}")))
}

fn loc(d: DirLoc) -> String {
    format!("{}/{}/{}", d.spc, d.db, d.relnumber)
}

fn image(t: Option<TupleImage<'_>>) -> String {
    t.map_or("none".to_string(), |t| t.data.len().to_string())
}

pub fn pgrc_desc(buf: &mut StringInfo<'_>, record: &XLogReaderState) -> PgResult<()> {
    let data = rec_data(record);
    match rec_info(record) & !XLR_INFO_MASK {
        XLOG_PGRC_DIR_CREATE | XLOG_PGRC_TRUNCATE => appendf!(buf, "dir {}", loc(decode_dir(data).map_err(bad)?)),
        XLOG_PGRC_FILE_WRITE | XLOG_PGRC_FILE_CREATE => {
            let r = FileWriteRec::decode(data).map_err(bad)?;
            appendf!(buf, "dir {} {} off {} len {}", loc(r.dir), r.name, r.off, r.bytes.len())
        }
        XLOG_PGRC_FILE_REF => {
            let r = FileRefRec::decode(data).map_err(bad)?;
            let masks = r.masks.iter().filter(|m| m.1 != 0).count();
            appendf!(buf, "dir {} {} len {} crc {:08X} masks {}", loc(r.dir), r.name, r.len, r.crc, masks)?;
            for (off, len) in r.masks.iter().filter(|m| m.1 != 0) {
                appendf!(buf, " [{off}+{len}]")?;
            }
            Ok(())
        }
        XLOG_PGRC_PUBLISH => {
            let r = PublishRec::decode(data).map_err(bad)?;
            appendf!(
                buf,
                "dir {} gen {} base {} kind {} publisher {} +{} -{} installs {} renames {} crc {:08X}",
                loc(r.dir),
                r.header.gen,
                r.base_gen,
                r.kind.name(),
                r.header.publisher_fxid,
                r.upsert.len(),
                r.retire.len(),
                r.installs.len(),
                r.renames.len(),
                r.manifest_crc
            )
        }
        XLOG_PGRC_INSTALL => {
            let r = InstallRec::decode(data).map_err(bad)?;
            appendf!(buf, "dir {} renames {}", loc(r.dir), r.renames.len())?;
            for (from, to) in &r.renames {
                appendf!(buf, "; {from} -> {to}")?;
            }
            Ok(())
        }
        XLOG_PGRC_RECLAIM => {
            let r = ReclaimRec::decode(data).map_err(bad)?;
            appendf!(buf, "dir {} xid {}", loc(r.dir), r.xid)?;
            match r.retire_below {
                Some(Retire::Conflict(g)) => appendf!(buf, " retire_below {g}")?,
                Some(Retire::Deferred(g)) => appendf!(buf, " retire_below {g} deferred")?,
                None => {}
            }
            appendf!(buf, " names {}", r.names.len())
        }
        op @ (XLOG_PGRC_ROW_DELETE | XLOG_PGRC_ROW_UPDATE) => {
            let r = RowRec::decode(op, data).map_err(bad)?;
            appendf!(buf, "parent {} old {}", loc(r.parent), image(r.old))?;
            if op == XLOG_PGRC_ROW_UPDATE {
                appendf!(buf, " new {}", image(r.new))?;
            }
            Ok(())
        }
        XLOG_PGRC_BULK_ROWS => {
            let r = BulkRowsRec::decode(data).map_err(bad)?;
            appendf!(buf, "parent {} ntuples {} bytes {}", loc(r.parent), r.ntuples, r.tuples.len())
        }
        XLOG_PGRC_ROWID_RESERVE | XLOG_PGRC_ROWID_SET => {
            let r = RowidReserveRec::decode(data).map_err(bad)?;
            appendf!(buf, "dir {} high {}", loc(r.dir), r.high)
        }
        XLOG_PGRC_ROWID_EXTENTS => {
            let r = RowidExtentsRec::decode(data).map_err(bad)?;
            let segs = r.table.iter().map(|e| e.part_no).collect::<std::collections::BTreeSet<_>>().len();
            appendf!(buf, "dir {} extents {} segments {}", loc(r.dir), r.table.len(), segs)
        }
        _ => Ok(()),
    }
}

pub fn pgrc_identify(info: u8) -> Option<&'static str> {
    identify(info & !XLR_INFO_MASK)
}
