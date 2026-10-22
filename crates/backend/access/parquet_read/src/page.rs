
use types_error::PgResult;

use crate::thrift::{corrupt, Cur, T_STRUCT};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Enc {
    Plain,
    PlainDictionary,
    Rle,
    BitPacked,
    DeltaBinaryPacked,
    DeltaLengthByteArray,
    DeltaByteArray,
    RleDictionary,
    ByteStreamSplit,
    Unknown(i32),
}

impl Enc {
    fn from_i32(v: i32) -> Enc {
        match v {
            0 => Enc::Plain,
            2 => Enc::PlainDictionary,
            3 => Enc::Rle,
            4 => Enc::BitPacked,
            5 => Enc::DeltaBinaryPacked,
            6 => Enc::DeltaLengthByteArray,
            7 => Enc::DeltaByteArray,
            8 => Enc::RleDictionary,
            9 => Enc::ByteStreamSplit,
            other => Enc::Unknown(other),
        }
    }

    pub fn name(self) -> String {
        match self {
            Enc::Plain => "PLAIN".into(),
            Enc::PlainDictionary => "PLAIN_DICTIONARY".into(),
            Enc::Rle => "RLE".into(),
            Enc::BitPacked => "BIT_PACKED".into(),
            Enc::DeltaBinaryPacked => "DELTA_BINARY_PACKED".into(),
            Enc::DeltaLengthByteArray => "DELTA_LENGTH_BYTE_ARRAY".into(),
            Enc::DeltaByteArray => "DELTA_BYTE_ARRAY".into(),
            Enc::RleDictionary => "RLE_DICTIONARY".into(),
            Enc::ByteStreamSplit => "BYTE_STREAM_SPLIT".into(),
            Enc::Unknown(v) => format!("encoding #{v}"),
        }
    }
}

pub(crate) enum PageKind {
    /// v1 data page: [rep levels][def levels][values], all under one codec.
    Data { num_values: usize, encoding: Enc, def_encoding: Enc, rep_encoding: Enc },
    /// v2 data page: uncompressed RLE levels of the given byte lengths ahead
    /// of the (optionally compressed) values.
    DataV2 {
        num_values: usize,
        num_nulls: usize,
        num_rows: usize,
        encoding: Enc,
        def_len: usize,
        rep_len: usize,
        compressed: bool,
    },
    Dict { num_values: usize, encoding: Enc },
    /// Index pages are skipped, not decoded.
    Skip,
}

pub(crate) struct PageHead {
    pub kind: PageKind,
    pub uncompressed_size: usize,
    pub compressed_size: usize,
    /// Offset of the first payload byte inside the chunk buffer.
    pub payload_at: usize,
    /// crc32 of the payload as stored, when the writer recorded one.
    pub crc: Option<u32>,
}

fn count(v: i32, what: &str) -> PgResult<usize> {
    usize::try_from(v).map_err(|_| corrupt(what))
}

/// Parse one page header at `at` inside `chunk`; the header's thrift bytes
/// are self-delimiting (fields until STOP).
pub(crate) fn parse_page_header(chunk: &[u8], at: usize, _column: &str) -> PgResult<PageHead> {
    let Some(rest) = chunk.get(at..) else {
        return Err(corrupt("page header offset beyond chunk"));
    };
    let mut cur = Cur::new(rest);

    let mut page_type: Option<i32> = None;
    let mut uncompressed: Option<i32> = None;
    let mut compressed: Option<i32> = None;
    let mut crc: Option<u32> = None;
    let mut data_hdr: Option<(usize, Enc, Enc, Enc)> = None;
    let mut dict_hdr: Option<(usize, Enc)> = None;
    let mut v2_hdr: Option<PageKind> = None;

    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            1 => page_type = Some(cur.i32_value(t)?),
            2 => uncompressed = Some(cur.i32_value(t)?),
            3 => compressed = Some(cur.i32_value(t)?),
            4 => crc = Some(cur.i32_value(t)? as u32),
            5 => {
                if t != T_STRUCT {
                    return Err(corrupt("data page header is not a struct"));
                }
                let mut num_values: Option<i32> = None;
                let mut enc: Enc = Enc::Plain;
                let mut def_enc: Enc = Enc::Rle;
                let mut rep_enc: Enc = Enc::Rle;
                let mut lid = 0i16;
                while let Some((ft, fid)) = cur.field(&mut lid)? {
                    match fid {
                        1 => num_values = Some(cur.i32_value(ft)?),
                        2 => enc = Enc::from_i32(cur.i32_value(ft)?),
                        3 => def_enc = Enc::from_i32(cur.i32_value(ft)?),
                        4 => rep_enc = Enc::from_i32(cur.i32_value(ft)?),
                        _ => cur.skip(ft, 0)?, // statistics etc.
                    }
                }
                let n = num_values.ok_or_else(|| corrupt("data page without num_values"))?;
                data_hdr = Some((count(n, "negative data page num_values")?, enc, def_enc, rep_enc));
            }
            7 => {
                if t != T_STRUCT {
                    return Err(corrupt("dictionary page header is not a struct"));
                }
                let mut num_values: Option<i32> = None;
                let mut enc: Enc = Enc::Plain;
                let mut lid = 0i16;
                while let Some((ft, fid)) = cur.field(&mut lid)? {
                    match fid {
                        1 => num_values = Some(cur.i32_value(ft)?),
                        2 => enc = Enc::from_i32(cur.i32_value(ft)?),
                        _ => cur.skip(ft, 0)?,
                    }
                }
                let n =
                    num_values.ok_or_else(|| corrupt("dictionary page without num_values"))?;
                dict_hdr = Some((count(n, "negative dictionary num_values")?, enc));
            }
            8 => {
                if t != T_STRUCT {
                    return Err(corrupt("data page v2 header is not a struct"));
                }
                let (mut nv, mut nn, mut nr) = (None, None, None);
                let mut enc = Enc::Plain;
                let (mut dl, mut rl) = (None, None);
                let mut compressed_flag = true;
                let mut lid = 0i16;
                while let Some((ft, fid)) = cur.field(&mut lid)? {
                    match fid {
                        1 => nv = Some(cur.i32_value(ft)?),
                        2 => nn = Some(cur.i32_value(ft)?),
                        3 => nr = Some(cur.i32_value(ft)?),
                        4 => enc = Enc::from_i32(cur.i32_value(ft)?),
                        5 => dl = Some(cur.i32_value(ft)?),
                        6 => rl = Some(cur.i32_value(ft)?),
                        7 => compressed_flag = cur.bool_value(ft)?,
                        _ => cur.skip(ft, 0)?,
                    }
                }
                let (Some(nv), Some(nn), Some(nr), Some(dl), Some(rl)) = (nv, nn, nr, dl, rl)
                else {
                    return Err(corrupt("data page v2 header missing a count"));
                };
                v2_hdr = Some(PageKind::DataV2 {
                    num_values: count(nv, "negative data page num_values")?,
                    num_nulls: count(nn, "negative data page num_nulls")?,
                    num_rows: count(nr, "negative data page num_rows")?,
                    encoding: enc,
                    def_len: count(dl, "negative definition level length")?,
                    rep_len: count(rl, "negative repetition level length")?,
                    compressed: compressed_flag,
                });
            }
            _ => cur.skip(t, 0)?,
        }
    }

    let (Some(page_type), Some(uncompressed), Some(compressed)) =
        (page_type, uncompressed, compressed)
    else {
        return Err(corrupt("page header missing type or sizes"));
    };
    if uncompressed < 0 || compressed < 0 {
        return Err(corrupt("negative page sizes"));
    }
    let payload_at = at + cur.pos();
    if payload_at + compressed as usize > chunk.len() {
        return Err(corrupt("page payload overruns chunk"));
    }

    let kind = match page_type {
        0 => {
            let (num_values, encoding, def_encoding, rep_encoding) =
                data_hdr.ok_or_else(|| corrupt("data page without its header"))?;
            PageKind::Data { num_values, encoding, def_encoding, rep_encoding }
        }
        1 => PageKind::Skip, // index page: skip payload, keep walking
        2 => {
            let (num_values, encoding) =
                dict_hdr.ok_or_else(|| corrupt("dictionary page without its header"))?;
            PageKind::Dict { num_values, encoding }
        }
        3 => {
            let kind = v2_hdr.ok_or_else(|| corrupt("data page v2 without its header"))?;
            if let PageKind::DataV2 { def_len, rep_len, num_nulls, num_values, .. } = &kind {
                if def_len + rep_len > compressed as usize
                    || def_len + rep_len > uncompressed as usize
                {
                    return Err(corrupt("data page v2 levels overrun the page"));
                }
                if num_nulls > num_values {
                    return Err(corrupt("data page v2 declares more nulls than values"));
                }
            }
            kind
        }
        _ => return Err(corrupt("unrecognized page type")),
    };

    Ok(PageHead {
        kind,
        uncompressed_size: uncompressed as usize,
        compressed_size: compressed as usize,
        payload_at,
        crc,
    })
}
