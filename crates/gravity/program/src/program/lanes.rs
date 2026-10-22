//! A site's lanes: `LaneId`, `LaneDesc` (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaneId {
    Attr(i16),
    RowId,
    Offset,
    Verdict(u8),
    Projection(u16),
    JsonbPath(u16),
    /// A dictionary entry table over a code lane (`gravity_dict::lane`): the site's `n`-th table.
    Table(u16),
}

impl LaneId {
    pub const fn encode(self) -> u32 {
        match self {
            LaneId::Attr(a) => lane::id(lane::ID_ATTR, a as u16 as u32),
            LaneId::RowId => lane::id(lane::ID_ROWID, 0),
            LaneId::Offset => lane::id(lane::ID_OFFSET, 0),
            LaneId::Verdict(n) => lane::id(lane::ID_VERDICT, n as u32),
            LaneId::Projection(n) => lane::id(lane::ID_PROJECTION, n as u32),
            LaneId::JsonbPath(n) => lane::id(lane::ID_JSONB, n as u32),
            LaneId::Table(n) => lane::id(lane::ID_TABLE, n as u32),
        }
    }

    pub const fn decode(id: u32) -> Option<LaneId> {
        let n = lane::id_ordinal(id);
        match lane::id_kind(id) {
            lane::ID_ATTR => Some(LaneId::Attr(n as u16 as i16)),
            lane::ID_ROWID => Some(LaneId::RowId),
            lane::ID_OFFSET => Some(LaneId::Offset),
            lane::ID_VERDICT => Some(LaneId::Verdict(n as u8)),
            lane::ID_PROJECTION => Some(LaneId::Projection(n as u16)),
            lane::ID_JSONB => Some(LaneId::JsonbPath(n as u16)),
            lane::ID_TABLE => Some(LaneId::Table(n as u16)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaneDesc {
    pub id: LaneId,
    pub kind: u8,
    pub width: u8,
    pub region: u8,
    /// A `numeric(p, s)` column's typmod facts (decompose `lane_of`); `NumTypmod::NONE` for every other lane.
    pub num: NumTypmod,
    pub rep: Rep,
    pub life: LaneLife,
}

impl LaneDesc {
    pub const NONE: LaneDesc = LaneDesc::value(LaneId::RowId, 0, 0, 0);

    pub(crate) const fn value(id: LaneId, kind: u8, width: u8, region: u8) -> LaneDesc {
        LaneDesc { id, kind, width, region, num: NumTypmod::NONE, rep: Rep::Word { bytes: if width == 0 { 8 } else { width } }, life: LaneLife::VALUE }
    }

    pub const fn attr(attno: i16) -> LaneDesc {
        LaneDesc::value(LaneId::Attr(attno), lane::KIND_BYVAL, 8, lane::REGION_PAGE)
    }

    pub const fn offset() -> LaneDesc {
        LaneDesc::value(LaneId::Offset, lane::KIND_BYVAL, 2, lane::REGION_ARENA)
    }

    pub const fn rowid() -> LaneDesc {
        LaneDesc::value(LaneId::RowId, lane::KIND_BYVAL, 8, lane::REGION_ARENA)
    }

    /// The site's `n`-th entry table, resident with the part's dictionary (`Site.lane(k)` fills it).
    pub const fn table(n: u16) -> LaneDesc {
        LaneDesc { life: LaneLife::DICTIONARY, ..LaneDesc::value(LaneId::Table(n), lane::KIND_BYVAL, 8, lane::REGION_DICTIONARY) }
    }
}
