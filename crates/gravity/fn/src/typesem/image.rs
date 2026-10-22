//! Built-in functions whose result reads a datum's stored image, not its value (D2764): the size, compression or TOAST
//! pointer of the bytes as stored. Two images of one value can answer differently: PostgreSQL's whole-row image
//! (`ExecEvalWholeRowVar`) keeps a dropped attribute's stored value, which every value reader skips (record_out /
//! record_send, record_eq / record_image_eq, hash_record, row_to_json), so only these see it.
type Oid = u32;

const F_PG_COLUMN_SIZE: Oid = 1269;
const F_PG_COLUMN_COMPRESSION: Oid = 2121;
const F_PG_COLUMN_TOAST_CHUNK_ID: Oid = 6316;

/// Function `funcid` reads its argument's stored image size: `pg_column_size`, `pg_column_compression`,
/// `pg_column_toast_chunk_id`.
pub fn reads_stored_image_size(funcid: Oid) -> bool {
    matches!(funcid, F_PG_COLUMN_SIZE | F_PG_COLUMN_COMPRESSION | F_PG_COLUMN_TOAST_CHUNK_ID)
}

/// Function `funcid` reads its argument's header width, which a tuple copy changes (D2954): `pg_column_size` answers 1 +
/// the data bytes for a varlena `heap_fill_tuple` packed to a 1-byte header and 4 + them for the same value unpacked; the
/// compression method and the TOAST pointer survive a copy (an inline compressed or external datum is copied as is).
pub fn reads_header_width(funcid: Oid) -> bool {
    funcid == F_PG_COLUMN_SIZE
}

/// Function `funcid` reads a size that is NULL for a NULL argument only and at least one byte for any other datum
/// (`pg_column_size`: strict, and every stored image takes a byte), so whether it is NULL and whether it is `> 0` are its
/// argument's NULL-ness whichever image it reads (D3374). `pg_column_compression` and `pg_column_toast_chunk_id` answer
/// NULL for an uncompressed / inline image, so their NULL-ness is the image's.
pub fn sized_when_set(funcid: Oid) -> bool {
    funcid == F_PG_COLUMN_SIZE
}

const F_INT4LT: Oid = 66;
const F_INT4GT: Oid = 147;

/// The operand int4 compare `funcid` finds positive when its other operand is the constant 0: `int4gt(x, 0)`'s first
/// (index 0), `int4lt(0, x)`'s second (index 1); `None` for any other function. A `sized_when_set` reader there is read
/// only for its NULL-ness (D3374).
pub fn positive_operand(funcid: Oid) -> Option<usize> {
    match funcid {
        F_INT4GT => Some(0),
        F_INT4LT => Some(1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_image_readers() {
        assert!(reads_stored_image_size(1269), "pg_column_size");
        assert!(reads_stored_image_size(2121), "pg_column_compression");
        assert!(reads_stored_image_size(6316), "pg_column_toast_chunk_id");
        assert!(!reads_stored_image_size(1257), "textlen: a value reader");
        assert!(!reads_stored_image_size(0), "no function");
    }

    #[test]
    fn only_the_size_reader_sees_a_copy_s_header() {
        assert!(reads_header_width(1269), "pg_column_size");
        assert!(!reads_header_width(2121), "pg_column_compression: a copy keeps the method");
        assert!(!reads_header_width(6316), "pg_column_toast_chunk_id: a copy keeps the pointer");
        assert!(!reads_header_width(1257), "textlen: a value reader");
    }

    #[test]
    fn only_the_size_reader_is_set_exactly_when_its_argument_is() {
        assert!(sized_when_set(1269), "pg_column_size");
        assert!(!sized_when_set(2121), "pg_column_compression: NULL for an uncompressed image");
        assert!(!sized_when_set(6316), "pg_column_toast_chunk_id: NULL for an inline image");
        assert!(!sized_when_set(1257), "textlen");
    }

    #[test]
    fn the_positive_operand_of_a_compare_with_zero() {
        assert_eq!(positive_operand(147), Some(0), "int4gt(x, 0)");
        assert_eq!(positive_operand(66), Some(1), "int4lt(0, x)");
        assert_eq!(positive_operand(150), None, "int4ge: x >= 0 is not x > 0");
        assert_eq!(positive_operand(65), None, "int4eq");
    }
}
