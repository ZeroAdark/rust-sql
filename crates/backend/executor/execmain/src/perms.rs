
use ::types_error::{PgError, PgResult};
use ::types_nodes::parsenodes::RTEPermissionInfo;

const ACL_INSERT: u64 = 1 << 0;
const ACL_SELECT: u64 = 1 << 1;
const ACLCHECK_OK: i32 = 0;

pub(crate) fn exec_check_permissions_over_perminfos(
    perm_infos: &::types_nodes::NodeList<'_>,
) -> PgResult<()> {
    for pi_node in perm_infos.iter() {
        let pi = pi_node.as_rte_permission_info().expect("permInfos cell");
        debug_assert!(pi.relid != 0);
        if !exec_check_one_rel_perms(pi)? {
            permission_denied(pi.relid)?;
        }
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn permission_denied(relid: ::types_core::Oid) -> PgResult<()> {
    use types_nodes::parsenodes::ObjectType;
    const RELKIND_SEQUENCE: i8 = b'S' as i8;
    const RELKIND_VIEW: i8 = b'v' as i8;
    const RELKIND_MATVIEW: i8 = b'm' as i8;
    const RELKIND_FOREIGN_TABLE: i8 = b'f' as i8;
    let shape = syscache_seams::lookup_pg_class_ls_shape::call(relid)?;
    let objtype = match shape.map(|s| s.relkind) {
        Some(RELKIND_SEQUENCE) => ObjectType::OBJECT_SEQUENCE,
        Some(RELKIND_VIEW) => ObjectType::OBJECT_VIEW,
        Some(RELKIND_MATVIEW) => ObjectType::OBJECT_MATVIEW,
        Some(RELKIND_FOREIGN_TABLE) => ObjectType::OBJECT_FOREIGN_TABLE,
        _ => ObjectType::OBJECT_TABLE,
    };
    let name = syscache_seams::pg_class_relname::call(relid)?;
    let name = name
        .as_ref()
        .map(|n| core::str::from_utf8(n.name_str()).unwrap_or(""))
        .unwrap_or("");
    aclchk_seams::aclcheck_error::call(1, objtype as i32, name)
}

pub fn exec_check_one_rel_perms(pi: &RTEPermissionInfo<'_>) -> PgResult<bool> {
    use types_nodes::parsenodes::ACL_UPDATE;
    const FIRST_LOW_INVALID_HEAP_ATTNUM: i32 = -7;

    let required = pi.requiredPerms;
    debug_assert!(required != 0);
    let userid = if pi.checkAsUser != 0 {
        pi.checkAsUser
    } else {
        miscinit_seams::get_user_id::call()
    };

    let rel_perms = aclchk_seams::pg_class_aclmask::call(pi.relid, userid, required, true)?;
    let remaining = required & !rel_perms;
    if remaining == 0 {
        return Ok(true);
    }

    if remaining & !(ACL_SELECT | ACL_INSERT | ACL_UPDATE) != 0 {
        return Ok(false);
    }

    if remaining & ACL_SELECT != 0 {
        if pi.selectedCols.is_empty()
            && aclchk_seams::pg_attribute_aclcheck_all::call(pi.relid, userid, ACL_SELECT, false)?
                != ACLCHECK_OK
        {
            return Ok(false);
        }
        let mut col = -1i32;
        loop {
            col = pi.selectedCols.next_member(col);
            if col < 0 {
                break;
            }
            let attno = col + FIRST_LOW_INVALID_HEAP_ATTNUM;
            if attno == 0 {
                if aclchk_seams::pg_attribute_aclcheck_all::call(
                    pi.relid, userid, ACL_SELECT, true,
                )? != ACLCHECK_OK
                {
                    return Ok(false);
                }
            } else if aclchk_seams::pg_attribute_aclcheck::call(
                pi.relid,
                attno as i16,
                userid,
                ACL_SELECT,
            )? != ACLCHECK_OK
            {
                return Ok(false);
            }
        }
    }

    if remaining & ACL_INSERT != 0
        && !exec_check_permissions_modified(pi.relid, userid, &pi.insertedCols, ACL_INSERT)?
    {
        return Ok(false);
    }
    if remaining & ACL_UPDATE != 0
        && !exec_check_permissions_modified(pi.relid, userid, &pi.updatedCols, ACL_UPDATE)?
    {
        return Ok(false);
    }
    Ok(true)
}

fn exec_check_permissions_modified(
    relid: ::types_core::Oid,
    userid: ::types_core::Oid,
    modified_cols: &::types_nodes::Bitmapset<'_>,
    required_perms: u64,
) -> PgResult<bool> {
    const FIRST_LOW_INVALID_HEAP_ATTNUM: i32 = -7;
    if modified_cols.is_empty() {
        return Ok(aclchk_seams::pg_attribute_aclcheck_all::call(
            relid,
            userid,
            required_perms,
            false,
        )? == ACLCHECK_OK);
    }
    let mut col = -1i32;
    loop {
        col = modified_cols.next_member(col);
        if col < 0 {
            break;
        }
        let attno = col + FIRST_LOW_INVALID_HEAP_ATTNUM;
        if attno == 0 {
            return Err(Box::new(PgError::error(
                "whole-row update is not implemented".to_string(),
            )));
        }
        if aclchk_seams::pg_attribute_aclcheck::call(relid, attno as i16, userid, required_perms)?
            != ACLCHECK_OK
        {
            return Ok(false);
        }
    }
    Ok(true)
}
