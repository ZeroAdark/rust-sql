seam_core::seam!(
    pub fn janitor_register() -> types_error::PgResult<()>
);

seam_core::seam!(
    // Ok(true): an idempotent Ensure was posted and the janitor completed the mint (or the
    //   database already existed): drain invalidations and retry the lookup ONCE.
    // Ok(false): minting is not armed for this name/role (feature off, prefix mismatch,
    //   unlisted role): fall through to the stock does-not-exist FATAL, byte-identical.
    // Err(_): a clean FATAL of its own: a malformed prefix-matching name from an authorized
    //   role (names the required template form), an absent janitor, wait timeout, per-role
    //   cap, or the janitor's saved CREATE DATABASE error fanned out to every waiter.
    pub fn ephemeral_db_mint_on_connect(dbname: &str) -> types_error::PgResult<bool>
);
