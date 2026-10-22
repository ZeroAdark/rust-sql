//! Sidecar companion files (spec §16 envelope `[SidecarFileHeader 48 B][payload][crc32c u32]`, named per spec §12
//! `part-<n>-<tag>-<gen>.pgrcs`).
//!
//! Stats companions are staged by the publisher: written to a tmp name through its logged vfs and renamed by the
//! generation's publish record ([`stage_stats_sidecars`]). No cache kind a reader builds is written: the Fp128 kind
//! stays reserved, and its tables live only in the reading process (USER 2026-10-20: the fingerprint seed is drawn per
//! process).
//!
//! Derived-kind discipline (spec §16): `part_uuid` binds the part's physical identity at build time — a copied or
//! rewritten part has a new identity and the reader treats the old sidecar as STALE (rebuild).

use pgrc_format::dirlayout;
use pgrc_format::sidecar::{SidecarFileHeader, SidecarKind, SIDECAR_MAGIC};
use pgrc_format::wire::crc32c;
use pgrc_format::FORMAT_VERSION;

use crate::wvfs::WriteVfs;
use crate::WriteResult;

/// Build the full companion-file image (header ‖ payload ‖ crc32c LE over
/// header+payload). Pure; the write path and the tests share it.
pub fn sidecar_file_image(
    kind: SidecarKind,
    part_uuid: [u8; 16],
    key_fingerprint: u64,
    payload: &[u8],
) -> Vec<u8> {
    sidecar_file_image_tail(kind, part_uuid, key_fingerprint, payload, &[])
}

/// `sidecar_file_image` of the payload `payload ‖ tail`, laid in one image
/// (a caller's fixed trailer needs no copy of its payload first).
pub fn sidecar_file_image_tail(
    kind: SidecarKind,
    part_uuid: [u8; 16],
    key_fingerprint: u64,
    payload: &[u8],
    tail: &[u8],
) -> Vec<u8> {
    let h = SidecarFileHeader {
        part_uuid,
        key_fingerprint,
        payload_len: (payload.len() + tail.len()) as u64,
        magic: SIDECAR_MAGIC,
        kind: kind.as_u16(),
        flags: 0,
        version: FORMAT_VERSION,
        pad: 0,
    };
    let mut out = Vec::with_capacity(48 + payload.len() + tail.len() + 4);
    h.encode_into(&mut out);
    out.extend_from_slice(payload);
    // a caller's fixed trailer, inside the exact capacity reserved above: never a reallocation
    out.extend_from_slice(tail);
    let crc = crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Stage the Stats companions of the parts a publish installs: each non-empty `(part_no, payload)` is written to
/// `tmp-<fxid>-<2^31 | part_no>` through `vfs`, and the `(tmp, final)` renames are returned for the publish record.
/// A part number is never reused within one transaction, so neither is the tmp name.
pub fn stage_stats_sidecars(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    schema_fingerprint: u64,
    pairs: &[(u32, &[u8])],
    fxid: u64,
) -> WriteResult<Vec<(String, String)>> {
    let mut renames = Vec::new();
    for (part_no, payload) in pairs {
        if payload.is_empty() {
            continue;
        }
        let image = sidecar_file_image(SidecarKind::Stats, [0u8; 16], schema_fingerprint, payload);
        let tmp_name = dirlayout::temp_file_name(fxid, 0x8000_0000 | *part_no);
        let fd = vfs.create_rw(&format!("{dir}/{tmp_name}"))?;
        vfs.pwrite_at(&fd, 0, &image)?;
        vfs.close_file(fd)?;
        renames.push((tmp_name, dirlayout::sidecar_file_name(*part_no, SidecarKind::Stats, 1)));
    }
    Ok(renames)
}

/// Stage one authoritative sidecar of a part a publish installs, at `tmp-<fxid>-<2^30 | part_no>`; the `(tmp,
/// final)` rename joins the publish record.
pub fn stage_sidecar(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    kind: SidecarKind,
    schema_fingerprint: u64,
    part_no: u32,
    payload: &[u8],
    fxid: u64,
) -> WriteResult<(String, String)> {
    stage_sidecar_gen(vfs, dir, kind, schema_fingerprint, part_no, 1, payload, fxid)
}

/// [`stage_sidecar`] named at generation `gen` (a DV at its `dv_gen`). One publish stages at most one authoritative
/// sidecar per part, so the tmp name stays unique.
#[allow(clippy::too_many_arguments)]
pub fn stage_sidecar_gen(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    kind: SidecarKind,
    schema_fingerprint: u64,
    part_no: u32,
    gen: u64,
    payload: &[u8],
    fxid: u64,
) -> WriteResult<(String, String)> {
    let image = sidecar_file_image(kind, [0u8; 16], schema_fingerprint, payload);
    let tmp_name = dirlayout::temp_file_name(fxid, 0x4000_0000 | part_no);
    let fd = vfs.create_rw(&format!("{dir}/{tmp_name}"))?;
    vfs.pwrite_at(&fd, 0, &image)?;
    vfs.close_file(fd)?;
    Ok((tmp_name, dirlayout::sidecar_file_name(part_no, kind, gen)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::sidecar::SidecarFileHeader;
    use pgrc_format::FormatError;

    const UUID: [u8; 16] = *b"part-uuid-16-byt";

    #[test]
    fn image_round_trips_through_the_frozen_validator() {
        let payload = b"memo payload bytes".to_vec();
        let image = sidecar_file_image(SidecarKind::Memo, UUID, 0xfeed_beef, &payload);
        let (h, body) = SidecarFileHeader::validate_file(&image).expect("valid image");
        assert_eq!(h.part_uuid, UUID);
        assert_eq!(h.key_fingerprint, 0xfeed_beef);
        assert_eq!(h.kind, SidecarKind::Memo.as_u16());
        assert_eq!(h.payload_len, payload.len() as u64);
        assert_eq!(body, &payload[..]);
    }

    #[test]
    fn corrupt_teeth_every_field_refuses_typed() {
        let image = sidecar_file_image(SidecarKind::Memo, UUID, 1, b"xyz");
        // crc flip
        let mut c = image.clone();
        let n = c.len();
        c[n - 1] ^= 0xff;
        assert!(matches!(
            SidecarFileHeader::validate_file(&c),
            Err(FormatError::CrcMismatch { .. })
        ));
        // magic flip
        let mut m = image.clone();
        m[32] ^= 0xff; // magic starts after part_uuid(16)+fingerprint(8)+len(8)
        assert!(matches!(
            SidecarFileHeader::validate_file(&m),
            Err(FormatError::BadMagic { .. })
        ));
        // payload_len lie
        let mut l = image.clone();
        l[24] ^= 0x01;
        assert!(SidecarFileHeader::validate_file(&l).is_err());
        // truncation
        assert!(matches!(
            SidecarFileHeader::validate_file(&image[..20]),
            Err(FormatError::Truncated { .. })
        ));
        // payload byte flip lands in the crc
        let mut p = image.clone();
        p[48] ^= 0xff;
        assert!(matches!(
            SidecarFileHeader::validate_file(&p),
            Err(FormatError::CrcMismatch { .. })
        ));
    }
}
