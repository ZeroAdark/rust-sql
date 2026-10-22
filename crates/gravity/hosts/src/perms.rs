use types_core::Oid;
use types_error::PgResult;
use types_nodes::parsenodes::{RTEPermissionInfo, ACL_INSERT, ACL_SELECT, ACL_UPDATE};
use types_nodes::Bitmapset;

pub const ACLCHECK_OK: i32 = 0;
const FIRST_LOW_INVALID_HEAP_ATTNUM: i32 = -7;

fn modified_ok(relid: Oid, userid: Oid, cols: &Bitmapset<'_>, required: u64) -> PgResult<bool> {
    if cols.is_empty() {
        return Ok(aclchk_seams::pg_attribute_aclcheck_all::call(relid, userid, required, false)? == ACLCHECK_OK);
    }
    let mut col = -1i32;
    loop {
        col = cols.next_member(col);
        if col < 0 {
            return Ok(true);
        }
        let attno = col + FIRST_LOW_INVALID_HEAP_ATTNUM;
        if attno == 0 {
            return Err(crate::pg_err("whole-row update is not implemented"));
        }
        if aclchk_seams::pg_attribute_aclcheck::call(relid, attno as i16, userid, required)? != ACLCHECK_OK {
            return Ok(false);
        }
    }
}

pub fn check_one_rel(pi: &RTEPermissionInfo<'_>) -> PgResult<bool> {
    let required = pi.requiredPerms;
    let userid = if pi.checkAsUser != 0 { pi.checkAsUser } else { miscinit_seams::get_user_id::call() };
    let rel_perms = aclchk_seams::pg_class_aclmask::call(pi.relid, userid, required, true)?;
    let remaining = required & !rel_perms;
    if remaining == 0 {
        return Ok(true);
    }
    if remaining & !(ACL_SELECT | ACL_INSERT | ACL_UPDATE) != 0 {
        return Ok(false);
    }
    if remaining & ACL_SELECT != 0 {
        if pi.selectedCols.is_empty() && aclchk_seams::pg_attribute_aclcheck_all::call(pi.relid, userid, ACL_SELECT, false)? != ACLCHECK_OK {
            return Ok(false);
        }
        let mut col = -1i32;
        loop {
            col = pi.selectedCols.next_member(col);
            if col < 0 {
                break;
            }
            let attno = col + FIRST_LOW_INVALID_HEAP_ATTNUM;
            let ok = if attno == 0 {
                aclchk_seams::pg_attribute_aclcheck_all::call(pi.relid, userid, ACL_SELECT, true)? == ACLCHECK_OK
            } else {
                aclchk_seams::pg_attribute_aclcheck::call(pi.relid, attno as i16, userid, ACL_SELECT)? == ACLCHECK_OK
            };
            if !ok {
                return Ok(false);
            }
        }
    }
    if remaining & ACL_INSERT != 0 && !modified_ok(pi.relid, userid, &pi.insertedCols, ACL_INSERT)? {
        return Ok(false);
    }
    if remaining & ACL_UPDATE != 0 && !modified_ok(pi.relid, userid, &pi.updatedCols, ACL_UPDATE)? {
        return Ok(false);
    }
    Ok(true)
}

pub fn check_all<'a>(infos: impl Iterator<Item = &'a RTEPermissionInfo<'a>>) -> PgResult<Option<Oid>> {
    for pi in infos {
        if pi.requiredPerms != 0 && !check_one_rel(pi)? {
            return Ok(Some(pi.relid));
        }
    }
    Ok(None)
}
