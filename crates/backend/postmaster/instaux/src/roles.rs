// Portions derived from CloudNativePG v1.30.1 (internal/management/controller/roles/
// contract.go, roles.go; pkg/postgres/roles.go), Copyright © contributors to
// CloudNativePG, established as CloudNativePG a Series of LF Projects, LLC. Licensed
// under the Apache License, Version 2.0. Modified by PagerFree, Inc., 2026: translated
// to Rust; applied in-engine with client-side SCRAM verifiers instead of SQL text.
// SPDX-License-Identifier: Apache-2.0

use cache_syscache::cacheinfo::{AUTHMEMMEMROLE, AUTHNAME, AUTHOID};
use cache_syscache::{ReleaseSysCache, ReleaseSysCacheList, SearchSysCache1, SearchSysCacheList1, SysCacheGetAttrNotNull, SysCacheKey};
use datum::Datum;
use mcx::Mcx;
use pgsync::Mutex;
use serde_json::Value;
use types_core::catalog::BOOTSTRAP_SUPERUSERID;
use types_core::Oid;
use types_error::PgResult;
use types_guc::{GucContext, GucSource};
use types_nodes::list::NodeList;
use types_nodes::parsenodes::{
    AccessPriv, AlterRoleStmt, CreateRoleStmt, DefElem, DefElemAction, DropBehavior, DropRoleStmt, GrantRoleStmt, RoleSpec,
    RoleSpecType, RoleStmtType,
};
use types_nodes::Node;

use crate::http::Response;
use crate::json::JsonOut;

const PG_EPOCH_UNIX: i64 = 946_684_800;
const DT_NOEND: i64 = i64::MAX;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Desired {
    pub name: String,
    pub present: bool,
    pub superuser: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub inherit: bool,
    pub login: bool,
    pub replication: bool,
    pub bypassrls: bool,
    pub connection_limit: i32,
    pub valid_until: Option<i64>,
    pub verifier: Option<Option<String>>,
    pub in_roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outcome {
    pub name: String,
    pub action: &'static str,
    pub verifier: Option<String>,
}

type Work = (Vec<Desired>, Option<Result<Vec<Outcome>, String>>);

static WORK: Mutex<Option<Work>> = Mutex::new(None);

pub(crate) fn is_reserved(name: &str) -> bool {
    name.starts_with("pg_")
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once(['T', ' '])?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, dd) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&dd) || h > 23 || mi > 59 || se > 60 || frac.len() > 6 {
        return None;
    }
    let micros: i64 = if frac.is_empty() { 0 } else { format!("{frac:0<6}").parse().ok()? };
    let unix = days_from_civil(y, mo, dd) * 86_400 + h * 3600 + mi * 60 + se;
    Some((unix - PG_EPOCH_UNIX) * 1_000_000 + micros)
}

pub(crate) fn pg_timestamp_text(ts: i64) -> String {
    let secs = ts.div_euclid(1_000_000) + PG_EPOCH_UNIX;
    let micros = ts.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{micros:06}+00", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub(crate) fn parse_request(body: &[u8]) -> Result<Vec<Desired>, Response> {
    let bad = |c: &str, m: String| Response::error(if c == "bad_request" { 400 } else { 422 }, c, &m);
    let v: Value = serde_json::from_slice(body).map_err(|e| bad("bad_request", format!("invalid JSON: {e}")))?;
    let Some(list) = v.get("roles").and_then(Value::as_array) else {
        return Err(bad("bad_request", "\"roles\" must be an array".into()));
    };
    let mut out = Vec::new();
    for r in list {
        let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() || name.len() > 63 || name.contains('\0') {
            return Err(bad("invalid_role", format!("invalid role name \"{name}\"")));
        }
        let flag = |k: &str, default: bool| r.get(k).and_then(Value::as_bool).unwrap_or(default);
        let present = match r.get("ensure").and_then(Value::as_str).unwrap_or("present") {
            "present" => true,
            "absent" => false,
            other => return Err(bad("invalid_role", format!("role \"{name}\": ensure must be present or absent, not \"{other}\""))),
        };
        let valid_until = match r.get("valid_until") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == "infinity" => None,
            Some(Value::String(s)) => Some(
                parse_rfc3339_utc(s)
                    .ok_or_else(|| bad("invalid_role", format!("role \"{name}\": valid_until must be an RFC 3339 UTC timestamp")))?,
            ),
            Some(_) => return Err(bad("invalid_role", format!("role \"{name}\": valid_until must be a string"))),
        };
        let verifier = match r.get("verifier") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => {
                if auth_scram::parse_scram_secret(s).is_none() {
                    return Err(bad("plaintext_password_refused", format!("role \"{name}\": verifier must be a SCRAM-SHA-256 verifier")));
                }
                Some(Some(s.clone()))
            }
            Some(_) => return Err(bad("invalid_role", format!("role \"{name}\": verifier must be a string or null"))),
        };
        let mut in_roles = Vec::new();
        for m in r.get("in_roles").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            match m.as_str() {
                Some(s) if !s.is_empty() && s.len() <= 63 => in_roles.push(s.to_string()),
                _ => return Err(bad("invalid_role", format!("role \"{name}\": in_roles must be role names"))),
            }
        }
        in_roles.sort();
        in_roles.dedup();
        let connection_limit = r.get("connection_limit").and_then(Value::as_i64).unwrap_or(-1).clamp(-1, i32::MAX as i64) as i32;
        out.push(Desired {
            name,
            present,
            superuser: flag("superuser", false),
            createdb: flag("createdb", false),
            createrole: flag("createrole", false),
            inherit: flag("inherit", true),
            login: flag("login", false),
            replication: flag("replication", false),
            bypassrls: flag("bypassrls", false),
            connection_limit,
            valid_until,
            verifier,
            in_roles,
        });
    }
    Ok(out)
}

pub(crate) fn begin(work: Vec<Desired>) -> PgResult<Option<bgworker::BackgroundWorkerHandle>> {
    *pgsync::lock(&WORK) = Some((work, None));
    bgworker::RegisterDynamicBackgroundWorker(bgworker::BackgroundWorker {
        bgw_name: "instance aux role apply".to_string(),
        bgw_type: "instance aux role apply".to_string(),
        bgw_flags: bgworker::BGWORKER_SHMEM_ACCESS | bgworker::BGWORKER_BACKEND_DATABASE_CONNECTION,
        bgw_start_time: bgworker::BgWorkerStartTime::RecoveryFinished,
        bgw_restart_time: bgworker::BGW_NEVER_RESTART,
        bgw_main: role_apply_main,
        bgw_main_arg: 0,
        bgw_extra: [0; bgworker::BGW_EXTRALEN],
        bgw_notify_pid: 0,
    })
}

pub(crate) fn take_result() -> Option<Result<Vec<Outcome>, String>> {
    pgsync::lock(&WORK).take().and_then(|(_, r)| r)
}

pub(crate) fn outcome_response(r: Result<Vec<Outcome>, String>) -> Response {
    match r {
        Err(m) => Response::error(422, "role_apply_failed", &m),
        Ok(list) => {
            let mut j = JsonOut::new();
            j.open(None);
            j.open_arr(Some("roles"));
            for o in &list {
                j.open(None);
                j.str(Some("name"), &o.name);
                j.str(Some("action"), o.action);
                j.opt_str(Some("verifier"), o.verifier.as_deref());
                j.close();
            }
            j.close_arr();
            j.close();
            Response::json(200, j.finish())
        }
    }
}

fn role_apply_main(_arg: u64) -> PgResult<()> {
    bgworker::BackgroundWorkerUnblockSignals();
    bgworker::BackgroundWorkerInitializeConnection(Some("postgres"), None, 0)?;
    let work = pgsync::lock(&WORK).as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
    let result = run(&work).map_err(|e| e.message().to_string());
    if xact::IsTransactionState() {
        let _ = xact::AbortCurrentTransaction();
    }
    if let Some(slot) = pgsync::lock(&WORK).as_mut() {
        slot.1 = Some(result);
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct Current {
    oid: Oid,
    superuser: bool,
    inherit: bool,
    createrole: bool,
    createdb: bool,
    login: bool,
    replication: bool,
    bypassrls: bool,
    connection_limit: i32,
    password: Option<String>,
    valid_until: Option<i64>,
    in_roles: Vec<String>,
}

fn role_name(oid: Oid) -> PgResult<Option<String>> {
    let Some(t) = SearchSysCache1(AUTHOID, SysCacheKey::Value(Datum::from_oid(oid)))? else { return Ok(None) };
    let d = SysCacheGetAttrNotNull(AUTHOID, &t, 2)?;
    // SAFETY: a Name column inside the held tuple: 64 bytes, NUL-terminated.
    let name = unsafe { core::ffi::CStr::from_ptr(d.as_usize() as *const core::ffi::c_char) }.to_string_lossy().into_owned();
    ReleaseSysCache(t);
    Ok(Some(name))
}

fn memberships(mcx: Mcx<'_>, member: Oid) -> PgResult<Vec<String>> {
    let list = SearchSysCacheList1(AUTHMEMMEMROLE, SysCacheKey::Value(Datum::from_oid(member)))?;
    let td = catcache::cache_tupdesc(AUTHMEMMEMROLE);
    let mut ids = Vec::new();
    for i in 0..list.n_members() as usize {
        let member = list.member(i);
        let tuple = member.tuple();
        if let Some(td) = td {
            let mut isnull = false;
            // SAFETY: a pg_auth_members tuple; roleid (2) is a fixed-width NOT NULL leading column.
            let d = unsafe { types_tuple::heap_getattr(&tuple, 2, td, &mut isnull) };
            ids.push(d.as_oid());
        }
    }
    ReleaseSysCacheList(list);
    let _ = mcx;
    let mut out = Vec::new();
    for id in ids {
        if let Some(n) = role_name(id)? {
            out.push(n);
        }
    }
    out.sort();
    Ok(out)
}

fn current(mcx: Mcx<'_>, name: &str) -> PgResult<Option<Current>> {
    let Some(t) = SearchSysCache1(AUTHNAME, SysCacheKey::Str(name))? else { return Ok(None) };
    let b = |n| SysCacheGetAttrNotNull(AUTHNAME, &t, n).map(|d| d.as_bool());
    let mut c = Current {
        oid: SysCacheGetAttrNotNull(AUTHNAME, &t, 1)?.as_oid(),
        superuser: b(3)?,
        inherit: b(4)?,
        createrole: b(5)?,
        createdb: b(6)?,
        login: b(7)?,
        replication: b(8)?,
        bypassrls: b(9)?,
        connection_limit: SysCacheGetAttrNotNull(AUTHNAME, &t, 10)?.as_i32(),
        ..Default::default()
    };
    ReleaseSysCache(t);
    if let Some(p) = syscache_seams::lookup_authid_rolpassword::call(mcx, name)? {
        c.password = p.rolpassword.map(|s| s.as_str().to_string());
        c.valid_until = p.rolvaliduntil.filter(|v| *v != DT_NOEND);
    }
    c.in_roles = memberships(mcx, c.oid)?;
    Ok(Some(c))
}

fn arena_str<'a>(mcx: Mcx<'a>, s: &str) -> PgResult<&'a str> {
    let mut p = mcx::PgString::new_in(mcx);
    p.try_push_str(s)?;
    Ok(core::str::from_utf8(p.into_bytes().leak()).unwrap_or(""))
}

struct Opts<'a> {
    list: NodeList<'a>,
}

impl<'a> Opts<'a> {
    fn push(&mut self, mcx: Mcx<'a>, name: &'a str, arg: Option<Node<'a>>) -> PgResult<()> {
        let d = Node::mk(mcx, DefElem { defnamespace: None, defname: Some(name), arg, defaction: DefElemAction::DEFELEM_UNSPEC, location: Default::default() })?;
        self.list.lappend(mcx, d)
    }
    fn flag(&mut self, mcx: Mcx<'a>, name: &'a str, v: bool) -> PgResult<()> {
        let arg = Node::mk_boolean(mcx, v)?;
        self.push(mcx, name, Some(arg))
    }
}

fn role_spec<'a>(mcx: Mcx<'a>, name: &'a str) -> PgResult<Node<'a>> {
    Node::mk(mcx, RoleSpec { roletype: RoleSpecType::ROLESPEC_CSTRING, rolename: Some(name), location: Default::default() })
}

fn grant<'a>(mcx: Mcx<'a>, member: &'a str, role: &'a str, is_grant: bool) -> PgResult<()> {
    let mut granted = NodeList::default();
    granted.lappend(mcx, Node::mk(mcx, AccessPriv { priv_name: Some(role), cols: NodeList::default() })?)?;
    let mut grantees = NodeList::default();
    grantees.lappend(mcx, role_spec(mcx, member)?)?;
    let stmt = GrantRoleStmt {
        granted_roles: granted,
        grantee_roles: grantees,
        is_grant,
        opt: NodeList::default(),
        grantor: None,
        behavior: DropBehavior::DROP_RESTRICT,
    };
    user::GrantRole(mcx, None, &stmt)
}

fn apply_one<'a>(mcx: Mcx<'a>, d: &Desired) -> PgResult<&'static str> {
    if is_reserved(&d.name) {
        return Ok("reserved");
    }
    let cur = current(mcx, &d.name)?;
    if !d.present {
        return match cur {
            None => Ok("unchanged"),
            Some(c) if c.oid == BOOTSTRAP_SUPERUSERID => Ok("reserved"),
            Some(_) => {
                let mut roles = NodeList::default();
                roles.lappend(mcx, role_spec(mcx, arena_str(mcx, &d.name)?)?)?;
                user::DropRole(mcx, &DropRoleStmt { roles, missing_ok: true })?;
                Ok("dropped")
            }
        };
    }
    let name = arena_str(mcx, &d.name)?;
    let until = match d.valid_until {
        Some(ts) => arena_str(mcx, &pg_timestamp_text(ts))?,
        None => "infinity",
    };
    let mut o = Opts { list: NodeList::default() };
    let Some(c) = cur else {
        for (n, v) in [("superuser", d.superuser), ("createdb", d.createdb), ("createrole", d.createrole), ("inherit", d.inherit), ("canlogin", d.login), ("isreplication", d.replication), ("bypassrls", d.bypassrls)] {
            o.flag(mcx, n, v)?;
        }
        o.push(mcx, "connectionlimit", Some(Node::mk_integer(mcx, d.connection_limit)?))?;
        o.push(mcx, "validUntil", Some(Node::mk_string(mcx, until)?))?;
        if let Some(Some(v)) = &d.verifier {
            o.push(mcx, "password", Some(Node::mk_string(mcx, arena_str(mcx, v)?)?))?;
        }
        user::CreateRole(mcx, None, &CreateRoleStmt { stmt_type: RoleStmtType::ROLESTMT_ROLE, role: Some(name), options: o.list })?;
        xact::CommandCounterIncrement()?;
        for r in &d.in_roles {
            grant(mcx, name, arena_str(mcx, r)?, true)?;
        }
        return Ok("created");
    };
    if c.oid == BOOTSTRAP_SUPERUSERID && (!d.superuser || !d.login) {
        return Ok("reserved");
    }
    let flags = [
        ("superuser", d.superuser, c.superuser),
        ("createdb", d.createdb, c.createdb),
        ("createrole", d.createrole, c.createrole),
        ("inherit", d.inherit, c.inherit),
        ("canlogin", d.login, c.login),
        ("isreplication", d.replication, c.replication),
        ("bypassrls", d.bypassrls, c.bypassrls),
    ];
    for (n, want, have) in flags {
        if want != have {
            o.flag(mcx, n, want)?;
        }
    }
    if d.connection_limit != c.connection_limit {
        o.push(mcx, "connectionlimit", Some(Node::mk_integer(mcx, d.connection_limit)?))?;
    }
    if d.valid_until != c.valid_until {
        o.push(mcx, "validUntil", Some(Node::mk_string(mcx, until)?))?;
    }
    match &d.verifier {
        Some(Some(v)) if c.password.as_deref() != Some(v.as_str()) => {
            o.push(mcx, "password", Some(Node::mk_string(mcx, arena_str(mcx, v)?)?))?;
        }
        Some(None) if c.password.is_some() => o.push(mcx, "password", None)?,
        _ => {}
    }
    let mut changed = !o.list.is_nil();
    if changed {
        let spec = RoleSpec { roletype: RoleSpecType::ROLESPEC_CSTRING, rolename: Some(name), location: Default::default() };
        user::AlterRole(mcx, None, &AlterRoleStmt { role: &spec, options: o.list, action: 1 })?;
        xact::CommandCounterIncrement()?;
    }
    for r in d.in_roles.iter().filter(|r| !c.in_roles.contains(r)) {
        grant(mcx, name, arena_str(mcx, r)?, true)?;
        changed = true;
    }
    for r in c.in_roles.iter().filter(|r| !d.in_roles.contains(r)) {
        grant(mcx, name, arena_str(mcx, r)?, false)?;
        changed = true;
    }
    Ok(if changed { "updated" } else { "unchanged" })
}

fn run(work: &[Desired]) -> PgResult<Vec<Outcome>> {
    let mut actions = Vec::new();
    xact::StartTransactionCommand()?;
    guc::SetConfigOption("search_path", Some("pg_catalog"), GucContext::PGC_SUSET, GucSource::PGC_S_SESSION)?;
    {
        let cx = mcx::MemoryContext::new("instance aux role apply");
        let mcx = cx.mcx();
        for d in work {
            actions.push(apply_one(mcx, d)?);
            xact::CommandCounterIncrement()?;
        }
    }
    xact::CommitTransactionCommand()?;
    xact::StartTransactionCommand()?;
    let cx = mcx::MemoryContext::new("instance aux role read-back");
    let mut out = Vec::new();
    for (d, action) in work.iter().zip(actions) {
        let verifier = if d.present && action != "reserved" {
            current(cx.mcx(), &d.name)?.and_then(|c| c.password).filter(|p| auth_scram::parse_scram_secret(p).is_some())
        } else {
            None
        };
        out.push(Outcome { name: d.name.clone(), action, verifier });
    }
    xact::CommitTransactionCommand()?;
    Ok(out)
}

pub(crate) fn refuse_in_recovery() -> Option<Response> {
    transam_xlog::RecoveryInProgress()
        .then(|| Response::error(409, "not_primary", "roles are applied on the primary only"))
}
