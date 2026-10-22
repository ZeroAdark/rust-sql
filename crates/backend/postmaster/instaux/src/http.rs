use std::io::{Read, Write};

pub(crate) const MAX_HEAD: usize = 16 * 1024;
pub(crate) const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug, PartialEq)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum HttpError {
    Closed,
    Malformed(&'static str),
    TooLarge,
    Unsupported(&'static str),
}

#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub(crate) fn json(status: u16, body: String) -> Self {
        Response { status, body }
    }

    pub(crate) fn error(status: u16, code: &str, message: &str) -> Self {
        Response { status, body: crate::json::error_body(code, message) }
    }
}

fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

pub(crate) fn read_request(r: &mut impl Read) -> Result<Request, HttpError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        if let Some(end) = head_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError::TooLarge);
        }
        let n = r.read(&mut chunk).map_err(|_| HttpError::Closed)?;
        if n == 0 {
            return Err(if buf.is_empty() { HttpError::Closed } else { HttpError::Malformed("truncated head") });
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let (method, target, length) = parse_head(&buf[..head_len])?;
    if length > MAX_BODY {
        return Err(HttpError::TooLarge);
    }
    let mut body = buf.split_off(head_len);
    while body.len() < length {
        let n = r.read(&mut chunk).map_err(|_| HttpError::Closed)?;
        if n == 0 {
            return Err(HttpError::Malformed("truncated body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let path = target.split('?').next().unwrap_or("").to_string();
    Ok(Request { method, path, body })
}

fn parse_head(raw: &[u8]) -> Result<(String, String, usize), HttpError> {
    let head = std::str::from_utf8(raw).map_err(|_| HttpError::Malformed("head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(HttpError::Malformed("request line"));
    };
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(HttpError::Malformed("request line"));
    }
    let mut length = 0usize;
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Malformed("header"));
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().map_err(|_| HttpError::Malformed("content-length"))?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(HttpError::Unsupported("transfer-encoding"));
        }
    }
    Ok((method.to_string(), target.to_string(), length))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

pub(crate) fn write_response(w: &mut impl Write, resp: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(resp.body.as_bytes())?;
    w.flush()
}

pub(crate) fn error_response(e: &HttpError) -> Option<Response> {
    match e {
        HttpError::Closed => None,
        HttpError::Malformed(what) => Some(Response::error(400, "bad_request", what)),
        HttpError::TooLarge => Some(Response::error(413, "too_large", "request exceeds the size limit")),
        HttpError::Unsupported(what) => Some(Response::error(501, "unsupported", what)),
    }
}
