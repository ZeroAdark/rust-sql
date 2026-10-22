seam_core::seam!(
    pub fn handle_slot_sync_message_interrupt()
);

seam_core::seam!(
    pub fn process_slot_sync_message() -> types_error::PgResult<()>
);
