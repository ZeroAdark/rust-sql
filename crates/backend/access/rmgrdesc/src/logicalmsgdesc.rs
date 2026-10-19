use crate::{appendf, rec_data, rec_info, Rec, XLR_INFO_MASK};
use stringinfo::StringInfo;
use types_error::PgResult;
use xlogreader_seams::XLogReaderState;

// xl_logical_message: dbId 0, transactional 4, prefix_size 8 (u64), message_size 16
// (u64), payload 24 (prefix incl. trailing NUL, then message bytes).
const XLOG_LOGICAL_MESSAGE: u8 = 0x00;

pub fn logicalmsg_desc(buf: &mut StringInfo<'_>, record: &XLogReaderState) -> PgResult<()> {
    logicalmsg_desc_data(buf, rec_data(record), rec_info(record) & !XLR_INFO_MASK)
}

fn logicalmsg_desc_data(buf: &mut StringInfo<'_>, data: &[u8], info: u8) -> PgResult<()> {
    let rec = Rec(data);

    if info == XLOG_LOGICAL_MESSAGE {
        let transactional = rec.u8(4, "xl_logical_message")? != 0;
        let prefix_size = rec.u64(8, "xl_logical_message")? as usize;
        let message_size = rec.u64(16, "xl_logical_message")? as usize;
        // The attacker controls prefix_size/message_size (64-bit fields read
        // straight from the record). Validate them against the actual payload
        // length with overflow-checked arithmetic before any slicing, and
        // require prefix_size >= 1 so the trailing-NUL trim (prefix_size - 1)
        // cannot underflow. On violation surface record_truncated instead of
        // panicking on an out-of-bounds slice.
        const HEADER: usize = 24;
        let payload_end = HEADER
            .checked_add(prefix_size)
            .and_then(|end| end.checked_add(message_size))
            .filter(|&end| prefix_size >= 1 && end <= rec.0.len())
            .ok_or_else(|| crate::record_truncated("xl_logical_message"))?;
        let prefix = rec
            .0
            .get(HEADER..HEADER + prefix_size)
            .ok_or_else(|| crate::record_truncated("xl_logical_message"))?;
        let message = rec
            .0
            .get(HEADER + prefix_size..payload_end)
            .ok_or_else(|| crate::record_truncated("xl_logical_message"))?;
        // C prints the prefix with %s: raw bytes up to the NUL, no encoding
        // check (a SQL_ASCII or LATIN1 database stores non-UTF-8 prefixes).
        let prefix = &prefix[..prefix_size - 1];
        let prefix = &prefix[..prefix.iter().position(|&b| b == 0).unwrap_or(prefix.len())];

        appendf!(
            buf,
            "{}, prefix \"",
            if transactional { "transactional" } else { "non-transactional" }
        )?;
        buf.append_bytes(prefix)?;
        appendf!(buf, "\"; payload ({message_size} bytes): ")?;
        let mut sep = "";
        for &byte in message {
            appendf!(buf, "{sep}{byte:02X}")?;
            sep = " ";
        }
    }
    Ok(())
}

pub fn logicalmsg_identify(info: u8) -> Option<&'static str> {
    if info & !XLR_INFO_MASK == XLOG_LOGICAL_MESSAGE {
        Some("MESSAGE")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{logicalmsg_desc_data, XLOG_LOGICAL_MESSAGE};
    use stringinfo::StringInfo;

    fn desc(prefix: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
        let mut data = vec![0u8; 24];
        data[8..16].copy_from_slice(&((prefix.len() + 1) as u64).to_ne_bytes());
        data[16..24].copy_from_slice(&(message.len() as u64).to_ne_bytes());
        data.extend_from_slice(prefix);
        data.push(0);
        data.extend_from_slice(message);
        let cx = mcx::MemoryContext::new("logicalmsgdesc_test");
        let mut buf = StringInfo::new_in(cx.mcx()).unwrap();
        logicalmsg_desc_data(&mut buf, &data, XLOG_LOGICAL_MESSAGE)
            .map_err(|e| e.to_string())?;
        Ok(buf.as_bytes().to_vec())
    }

    /// C parity = logicalmsgdesc.c %s: a non-UTF-8 prefix (LATIN1 "é" from
    /// a SQL_ASCII database) is printed byte for byte, never rejected.
    #[test]
    fn non_utf8_prefix_prints_raw_bytes() {
        assert_eq!(
            desc(b"\xe9", b"AB").unwrap(),
            b"non-transactional, prefix \"\xe9\"; payload (2 bytes): 41 42".to_vec()
        );
        assert_eq!(
            desc(b"pfx", b"").unwrap(),
            b"non-transactional, prefix \"pfx\"; payload (0 bytes): ".to_vec()
        );
    }

    #[test]
    fn truncated_prefix_still_errors() {
        let mut data = vec![0u8; 24];
        data[8..16].copy_from_slice(&4u64.to_ne_bytes());
        let cx = mcx::MemoryContext::new("logicalmsgdesc_test");
        let mut buf = StringInfo::new_in(cx.mcx()).unwrap();
        let err = logicalmsg_desc_data(&mut buf, &data, XLOG_LOGICAL_MESSAGE).unwrap_err();
        assert!(err.to_string().contains("xl_logical_message"));
    }
}
