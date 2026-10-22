//! OpenLDAP's `ldap_url_parse` (historic-flags shim) and `ldap_err2string`,
//! transcribed for the hba `ldapurl` option and the in-tree LDAP client.
//! RFC 4516 subset: scheme://host:port/dn?attrs?scope?filter?exts. Historic
//! semantics: empty host becomes absent, an empty dn after '/' stays "",
//! a missing port defaults per scheme, missing scope defaults to base.

// ldap.h LDAP_PORT / LDAPS_PORT.
pub const LDAP_PORT: i32 = 389;
pub const LDAPS_PORT: i32 = 636;

// LDAP result codes this build can produce (ldap.h).
pub const LDAP_SUCCESS: i32 = 0;

// ldap_url_parse error codes (ldap.h LDAP_URL_ERR_*). C hba.c formats them
// through ldap_err2string, which reads them as LDAP result codes — the
// nonsensical pairings below ("Time limit exceeded" for a bad scheme) are
// C-exact behavior.
pub const LDAP_URL_ERR_MEM: i32 = 0x01;
pub const LDAP_URL_ERR_PARAM: i32 = 0x02;
pub const LDAP_URL_ERR_BADSCHEME: i32 = 0x03;
pub const LDAP_URL_ERR_BADENCLOSURE: i32 = 0x04;
pub const LDAP_URL_ERR_BADURL: i32 = 0x05;
pub const LDAP_URL_ERR_BADHOST: i32 = 0x06;
pub const LDAP_URL_ERR_BADATTRS: i32 = 0x07;
pub const LDAP_URL_ERR_BADSCOPE: i32 = 0x08;
pub const LDAP_URL_ERR_BADFILTER: i32 = 0x09;
pub const LDAP_URL_ERR_BADEXTS: i32 = 0x0a;

/// OpenLDAP ldap_err2string (libldap/error.c), the subset of codes reachable
/// from this build's client plus the URL-parse pass-throughs.
pub fn ldap_err2string(code: i32) -> &'static str {
    match code {
        0 => "Success",
        1 => "Operations error",
        2 => "Protocol error",
        3 => "Time limit exceeded",
        4 => "Size limit exceeded",
        5 => "Compare False",
        6 => "Compare True",
        7 => "Authentication method not supported",
        8 => "Strong(er) authentication required",
        9 => "Partial results and referral received",
        10 => "Referral",
        11 => "Administrative limit exceeded",
        12 => "Critical extension is unavailable",
        13 => "Confidentiality required",
        14 => "SASL bind in progress",
        16 => "No such attribute",
        17 => "Undefined attribute type",
        18 => "Inappropriate matching",
        19 => "Constraint violation",
        20 => "Type or value exists",
        21 => "Invalid syntax",
        32 => "No such object",
        33 => "Alias problem",
        34 => "Invalid DN syntax",
        36 => "Alias dereferencing problem",
        48 => "Inappropriate authentication",
        49 => "Invalid credentials",
        50 => "Insufficient access",
        51 => "Server is busy",
        52 => "Server is unavailable",
        53 => "Server is unwilling to perform",
        54 => "Loop detected",
        64 => "Naming violation",
        65 => "Object class violation",
        66 => "Operation not allowed on non-leaf",
        67 => "Operation not allowed on RDN",
        68 => "Already exists",
        69 => "Cannot modify object class",
        70 => "Results too large",
        71 => "Affects multiple DSA's",
        80 => "Internal (implementation specific) error",
        -1 => "Can't contact LDAP server",
        -2 => "Local error",
        -3 => "Encoding error",
        -4 => "Decoding error",
        -5 => "Timed out",
        -6 => "Unknown authentication method",
        -7 => "Bad search filter",
        -8 => "User canceled operation",
        -9 => "Bad parameter to an ldap routine",
        -10 => "Out of memory",
        -11 => "Connect error",
        -12 => "Not Supported",
        -13 => "Control not found",
        -14 => "No results returned",
        -15 => "More results to return",
        -16 => "Client Loop",
        -17 => "Referral Limit Exceeded",
        _ => "Unknown error",
    }
}

#[derive(Debug, Clone, Default)]
pub struct LdapUrlDesc {
    pub scheme: String,
    pub host: Option<String>,
    pub port: i32,
    pub dn: Option<String>,
    pub attrs: Vec<String>,
    pub scope: i32,
    pub filter: Option<String>,
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ldap_pvt_hex_unescape: %hh percent-decoding; a '%' not followed by two hex
// digits is dropped with its trailing garbage kept, but we keep it literal
// (the C behavior is unspecified byte salad; literal is the defensible arm).
fn hex_unescape(s: &str) -> Result<String, i32> {
    let b = s.as_bytes();
    let mut out = ::mcx::std_vec_with_capacity("hba", b.len()).map_err(|_| LDAP_URL_ERR_MEM)?;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (
                b.get(i + 1).copied().and_then(hex_val),
                b.get(i + 2).copied().and_then(hex_val),
            ) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out.truncate(out.iter().position(|&c| c == 0).unwrap_or(out.len()));
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn str2scope(s: &str) -> Option<i32> {
    // OpenLDAP url.c str2scope.
    if s.eq_ignore_ascii_case("one") || s.eq_ignore_ascii_case("onelevel") {
        Some(types_startup::LDAP_SCOPE_ONELEVEL)
    } else if s.eq_ignore_ascii_case("base") || s.eq_ignore_ascii_case("exact") {
        Some(types_startup::LDAP_SCOPE_BASE)
    } else if s.eq_ignore_ascii_case("sub") || s.eq_ignore_ascii_case("subtree") {
        Some(types_startup::LDAP_SCOPE_SUBTREE)
    } else if s.eq_ignore_ascii_case("subordinates") || s.eq_ignore_ascii_case("children") {
        Some(types_startup::LDAP_SCOPE_SUBORDINATE)
    } else {
        None
    }
}

/// ldap_url_parse with LDAP_PVT_URL_PARSE_HISTORIC. Err is an LDAP_URL_ERR_*
/// code for hba's `could not parse LDAP URL` message.
pub fn ldap_url_parse(url: &str) -> Result<LdapUrlDesc, i32> {
    // skip_url_prefix: an optional "<...>" enclosure and a case-insensitive
    // "URL:" prefix precede the scheme.
    let (url, enclosed) = match url.strip_prefix('<') {
        Some(rest) => (rest, true),
        None => (url, false),
    };
    let url = if url.len() >= 4 && url[..4].eq_ignore_ascii_case("URL:") {
        &url[4..]
    } else {
        url
    };
    let Some(pos) = url.find("://") else {
        return Err(LDAP_URL_ERR_BADSCHEME);
    };
    let scheme_raw = &url[..pos];
    let scheme = if scheme_raw.eq_ignore_ascii_case("ldap") {
        "ldap"
    } else if scheme_raw.eq_ignore_ascii_case("ldaps") {
        "ldaps"
    } else if scheme_raw.eq_ignore_ascii_case("ldapi") {
        "ldapi"
    } else if scheme_raw.eq_ignore_ascii_case("cldap") {
        "cldap"
    } else {
        return Err(LDAP_URL_ERR_BADSCHEME);
    };

    let rest = &url[pos + 3..];
    let rest = if enclosed {
        rest.strip_suffix('>').ok_or(LDAP_URL_ERR_BADENCLOSURE)?
    } else {
        rest
    };
    let (hostport, sections) = match rest.find('/') {
        Some(slash) => (&rest[..slash], Some(&rest[slash + 1..])),
        None => (rest, None),
    };

    let mut desc = LdapUrlDesc {
        scheme: scheme.to_string(),
        scope: types_startup::LDAP_SCOPE_BASE,
        ..Default::default()
    };

    // host[:port], with [v6]:port bracket form.
    let (host_part, port_part): (&str, Option<&str>) = if let Some(h) = hostport.strip_prefix('[')
    {
        let Some(close) = h.find(']') else {
            return Err(LDAP_URL_ERR_BADENCLOSURE);
        };
        let after = &h[close + 1..];
        match after.strip_prefix(':') {
            Some(p) => (&h[..close], Some(p)),
            None if after.is_empty() => (&h[..close], None),
            None => return Err(LDAP_URL_ERR_BADURL),
        }
    } else {
        match hostport.find(':') {
            Some(colon) => (&hostport[..colon], Some(&hostport[colon + 1..])),
            None => (hostport, None),
        }
    };
    if !host_part.is_empty() {
        desc.host = Some(hex_unescape(host_part)?);
    }
    if let Some(p) = port_part {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(LDAP_URL_ERR_BADURL);
        }
        match p.parse::<u32>() {
            Ok(v) if v <= 65535 => desc.port = v as i32,
            _ => return Err(LDAP_URL_ERR_BADURL),
        }
    }

    // LDAP_PVT_URL_PARSE_DEF_PORT (part of HISTORIC).
    if desc.port == 0 && desc.scheme != "ldapi" {
        desc.port = if desc.scheme == "ldaps" { LDAPS_PORT } else { LDAP_PORT };
    }

    let Some(sections) = sections else {
        return Ok(desc);
    };
    let mut it = sections.splitn(5, '?');
    // HISTORIC lacks NOEMPTY_DN: "ldap://h/" yields dn "", not NULL.
    let dn = it.next().unwrap_or("");
    desc.dn = Some(hex_unescape(dn)?);
    if let Some(attrs) = it.next() {
        for a in attrs.split(',') {
            if !a.is_empty() {
                desc.attrs.push(hex_unescape(a)?);
            }
        }
    }
    if let Some(scope) = it.next() {
        if !scope.is_empty() {
            desc.scope = str2scope(scope).ok_or(LDAP_URL_ERR_BADSCOPE)?;
        }
    }
    if let Some(filter) = it.next() {
        if !filter.is_empty() {
            desc.filter = Some(hex_unescape(filter)?);
        }
    }
    if let Some(exts) = it.next() {
        // lud_exts: hba.c copies none of them, so they are validated and
        // dropped (an extra '?' is BADURL; only empty tokens is BADEXTS).
        if !exts.is_empty() {
            if exts.contains('?') {
                return Err(LDAP_URL_ERR_BADURL);
            }
            if exts.split(',').all(str::is_empty) {
                return Err(LDAP_URL_ERR_BADEXTS);
            }
            for e in exts.split(',').filter(|e| !e.is_empty()) {
                hex_unescape(e)?;
            }
        }
    }
    Ok(desc)
}

#[cfg(test)]
mod tests {
    use super::{
        ldap_url_parse, LDAP_URL_ERR_BADENCLOSURE, LDAP_URL_ERR_BADEXTS, LDAP_URL_ERR_BADURL,
    };

    // fp-libpq-hba#2: OpenLDAP skip_url_prefix takes "<...>" and "URL:".
    #[test]
    fn enclosure_and_url_prefix_are_skipped() {
        let d = ldap_url_parse("<URL:ldap://h:389/dc=a?uid?sub?(x=y)>").unwrap();
        assert_eq!(d.host.as_deref(), Some("h"));
        assert_eq!(d.port, 389);
        assert_eq!(d.dn.as_deref(), Some("dc=a"));
        assert_eq!(d.filter.as_deref(), Some("(x=y)"));
        assert_eq!(ldap_url_parse("url:LDAPS://h").unwrap().scheme, "ldaps");
        assert_eq!(ldap_url_parse("<ldap://h/>").unwrap().host.as_deref(), Some("h"));
        assert_eq!(ldap_url_parse("<ldap://h").err(), Some(LDAP_URL_ERR_BADENCLOSURE));
    }

    // fp-libpq-hba#3: extensions parse into lud_exts, which hba.c ignores.
    #[test]
    fn extensions_are_accepted_and_dropped() {
        let d = ldap_url_parse("ldap://h/dc=a?uid?sub?(x=y)?bindname=cn%3Dx,!x-foo").unwrap();
        assert_eq!(d.filter.as_deref(), Some("(x=y)"));
        assert_eq!(d.attrs, vec!["uid".to_string()]);
        assert!(ldap_url_parse("ldap://h/dc=a????").is_ok());
        assert_eq!(ldap_url_parse("ldap://h/dc=a????,").err(), Some(LDAP_URL_ERR_BADEXTS));
        assert_eq!(ldap_url_parse("ldap://h/dc=a????a?b").err(), Some(LDAP_URL_ERR_BADURL));
    }

    // ldap_pvt_hex_unescape writes into a C string: a %00 ends the component
    // (hbafuncs.c:109 formats ldapbasedn with %s).
    #[test]
    fn percent_nul_ends_the_component() {
        let d = ldap_url_parse("ldap://h%00x:389/dc=a%00,dc=b?uid%00x?sub?(x=%00y)").unwrap();
        assert_eq!(d.host.as_deref(), Some("h"));
        assert_eq!(d.dn.as_deref(), Some("dc=a"));
        assert_eq!(d.attrs, vec!["uid".to_string()]);
        assert_eq!(d.filter.as_deref(), Some("(x="));
        assert_eq!(d.scope, types_startup::LDAP_SCOPE_SUBTREE);
    }

    // ldap_url_parse's HISTORIC flags: DEF_PORT fills 389/636, and without
    // NOEMPTY_DN an empty dn after '/' is "" (hba.c:2226 keeps it as
    // ldapbasedn). The port colon is the first one (strchr).
    #[test]
    fn historic_flags_default_port_and_keep_empty_dn() {
        let d = ldap_url_parse("ldap://h/?uid?sub").unwrap();
        assert_eq!(d.port, 389);
        assert_eq!(d.dn.as_deref(), Some(""));
        assert_eq!(ldap_url_parse("ldaps://h/dc=x").unwrap().port, 636);
        assert_eq!(ldap_url_parse("ldap://h:0/dc=x").unwrap().port, 389);
        assert_eq!(ldap_url_parse("ldap://h:1234").unwrap().port, 1234);
        assert_eq!(ldap_url_parse("ldap://h").unwrap().dn, None);
        assert_eq!(ldap_url_parse("ldap://a:b:389/").err(), Some(LDAP_URL_ERR_BADURL));
    }
}
