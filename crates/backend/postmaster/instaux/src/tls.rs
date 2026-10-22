use openssl::asn1::Asn1Time;
use openssl::ssl::{
    SslAcceptor, SslContextBuilder, SslFiletype, SslMethod, SslOptions, SslRef, SslSessionCacheMode, SslVerifyMode,
};
use openssl::x509::{X509Name, X509Ref};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TlsFiles {
    pub cert: String,
    pub key: String,
    pub ca: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CertInfo {
    pub serial: String,
    pub not_after: i64,
}

pub(crate) fn files_from_gucs() -> TlsFiles {
    use guc_tables::backing as b;
    TlsFiles {
        cert: b::pgrust_instance_aux_cert_file().unwrap_or_default(),
        key: b::pgrust_instance_aux_key_file().unwrap_or_default(),
        ca: b::pgrust_instance_aux_ca_file().unwrap_or_default(),
    }
}

pub(crate) fn build_acceptor(f: &TlsFiles) -> Result<SslAcceptor, String> {
    if f.cert.is_empty() || f.key.is_empty() || f.ca.is_empty() {
        return Err("pgrust.instance_aux_cert_file, pgrust.instance_aux_key_file and pgrust.instance_aux_ca_file must all be set".into());
    }
    let mut b = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(|e| e.to_string())?;
    b.set_certificate_chain_file(&f.cert)
        .map_err(|e| format!("could not load certificate file \"{}\": {e}", f.cert))?;
    b.set_private_key_file(&f.key, SslFiletype::PEM)
        .map_err(|e| format!("could not load private key file \"{}\": {e}", f.key))?;
    b.check_private_key().map_err(|e| format!("private key does not match the certificate: {e}"))?;
    b.set_ca_file(&f.ca).map_err(|e| format!("could not load CA file \"{}\": {e}", f.ca))?;
    let names = X509Name::load_client_ca_file(&f.ca).map_err(|e| format!("could not load CA file \"{}\": {e}", f.ca))?;
    b.set_client_ca_list(names);
    b.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    // No resumption: every connection verifies the client certificate it is authorized by.
    b.set_num_tickets(0).map_err(|e| e.to_string())?;
    b.set_options(SslOptions::NO_TICKET);
    b.set_session_cache_mode(SslSessionCacheMode::OFF);
    Ok(b.build())
}

pub(crate) fn info_of(x: &X509Ref) -> Option<CertInfo> {
    let serial = x.serial_number().to_bn().ok()?.to_hex_str().ok()?.to_string();
    let d = Asn1Time::from_unix(0).ok()?.diff(x.not_after()).ok()?;
    Some(CertInfo { serial, not_after: d.days as i64 * 86_400 + d.secs as i64 })
}

pub(crate) fn acceptor_info(a: &SslAcceptor) -> Option<CertInfo> {
    a.context().certificate().and_then(info_of)
}

pub(crate) fn file_info(path: &str) -> Option<CertInfo> {
    if path.is_empty() {
        return None;
    }
    let mut b = SslContextBuilder::new(SslMethod::tls_server()).ok()?;
    b.set_certificate_chain_file(path).ok()?;
    b.build().certificate().and_then(info_of)
}

pub(crate) fn peer_identities(ssl: &SslRef) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(names) = ssl.peer_certificate().and_then(|c| c.subject_alt_names()) {
        for n in names.iter() {
            if let Some(d) = n.dnsname().or_else(|| n.uri()) {
                out.push(d.to_string());
            }
        }
    }
    out
}

pub(crate) fn admin_allowed(ids: &[String], patterns: &str) -> bool {
    patterns
        .split(',')
        .map(|p| p.trim().trim_matches('"'))
        .filter(|p| !p.is_empty())
        .any(|p| {
            ids.iter().any(|id| match p.strip_suffix('*') {
                Some(prefix) => id.starts_with(prefix),
                None => id == p,
            })
        })
}

pub(crate) fn reload_response() -> crate::http::Response {
    match build_acceptor(&files_from_gucs()) {
        Err(m) => crate::http::Response::error(422, "tls_invalid", &m),
        Ok(a) => {
            let aux = acceptor_info(&a);
            crate::shared::install_acceptor(a);
            postmaster_seams::signal_postmaster_sighup::call();
            let server = if guc_tables::vars::EnableSSL.read() {
                file_info(&guc_tables::vars::ssl_cert_file.read().unwrap_or_default())
            } else {
                None
            };
            let mut j = crate::json::JsonOut::new();
            j.open(None);
            for (key, info) in [("aux", aux), ("server", server)] {
                match info {
                    None => j.null(Some(key)),
                    Some(c) => {
                        j.open(Some(key));
                        j.str(Some("serial"), &c.serial);
                        j.int(Some("not_after"), c.not_after);
                        j.close();
                    }
                }
            }
            j.close();
            crate::http::Response::json(200, j.finish())
        }
    }
}
