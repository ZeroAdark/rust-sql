use gravity_abi::limits::MAX_CONVERSION_GROWTH;
use gravity_program::OutCol;
use mcx::Mcx;
use pqformat::{pq_beginmessage_reuse, pq_endmessage_reuse, pq_sendint16, pq_writeint16, pq_writeint32, pq_writestring};
use stringinfo::StringInfo;
use types_core::NAMEDATALEN;
use types_error::PgResult;

const PQMSG_ROW_DESCRIPTION: u8 = b'T';

pub struct RowDesc {
    buf: StringInfo<'static>,
}

impl RowDesc {
    /// `format(i)`: column i's format code (0 text, 1 binary).
    pub fn build(mcx: Mcx<'static>, out: &[OutCol], format: impl Fn(usize) -> i16) -> PgResult<RowDesc> {
        let mut buf = StringInfo::new_in(mcx)?;
        let natts = out.len();
        pq_beginmessage_reuse(&mut buf, PQMSG_ROW_DESCRIPTION);
        pq_sendint16(&mut buf, natts as u16)?;
        buf.enlarge((NAMEDATALEN as usize * MAX_CONVERSION_GROWTH + 4 + 2 + 4 + 2 + 4 + 2) * natts)?;
        for (i, col) in out.iter().enumerate() {
            let (typid, typmod) = lsyscache_seams::get_base_type_and_typmod::call(col.typid, col.typmod)?;
            let format = format(i);
            pq_writestring(&mut buf, col.name_bytes())?;
            pq_writeint32(&mut buf, col.origtbl);
            pq_writeint16(&mut buf, col.origcol as u16);
            pq_writeint32(&mut buf, typid);
            pq_writeint16(&mut buf, col.typlen as u16);
            pq_writeint32(&mut buf, typmod as u32);
            pq_writeint16(&mut buf, format as u16);
        }
        Ok(RowDesc { buf })
    }

    pub fn body(&self) -> &[u8] {
        self.buf.as_bytes()
    }

    pub fn send(&self) -> PgResult<()> {
        pq_endmessage_reuse(&self.buf)
    }
}
