seam_core::seam!(
    // StartupReorderBuffer (reorderbuffer.c): remove serialized spill files.
    pub fn startup_reorder_buffer() -> types_error::PgResult<()>
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionKind {
    /// `pg_toast.pgrc_seg_<parent>_<slot>`: its rows are the parent's; replay emits them on the parent.
    Segment { slot: u16 },
    /// `pg_toast.pgrc_tomb_<parent>`: never decoded (the row changes travel in pgrc's own records).
    Tombstone,
}

seam_core::seam!(
    // Uninstalled (no pgrcolumnar in the binary): replay treats nothing as a companion.
    pub fn pgrc_companion_of(
        relnamespace: types_core::Oid,
        relname: &[u8],
        relkind: u8,
    ) -> Option<(types_core::Oid, CompanionKind)>
);
