use std::fmt::Write;

pub(crate) struct JsonOut {
    buf: String,
    comma: bool,
}

pub(crate) fn lsn_text(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn as u32)
}

impl JsonOut {
    pub(crate) fn new() -> Self {
        JsonOut { buf: String::new(), comma: false }
    }

    fn key(&mut self, key: Option<&str>) {
        if self.comma {
            self.buf.push(',');
        }
        if let Some(k) = key {
            push_str_lit(&mut self.buf, k);
            self.buf.push(':');
        }
        self.comma = true;
    }

    pub(crate) fn open(&mut self, key: Option<&str>) {
        self.key(key);
        self.buf.push('{');
        self.comma = false;
    }

    pub(crate) fn close(&mut self) {
        self.buf.push('}');
        self.comma = true;
    }

    pub(crate) fn open_arr(&mut self, key: Option<&str>) {
        self.key(key);
        self.buf.push('[');
        self.comma = false;
    }

    pub(crate) fn close_arr(&mut self) {
        self.buf.push(']');
        self.comma = true;
    }

    pub(crate) fn str(&mut self, key: Option<&str>, v: &str) {
        self.key(key);
        push_str_lit(&mut self.buf, v);
    }

    pub(crate) fn opt_str(&mut self, key: Option<&str>, v: Option<&str>) {
        match v {
            Some(s) => self.str(key, s),
            None => self.null(key),
        }
    }

    pub(crate) fn int(&mut self, key: Option<&str>, v: i64) {
        self.key(key);
        let _ = write!(self.buf, "{v}");
    }

    pub(crate) fn uint(&mut self, key: Option<&str>, v: u64) {
        self.key(key);
        let _ = write!(self.buf, "{v}");
    }

    pub(crate) fn opt_uint(&mut self, key: Option<&str>, v: Option<u64>) {
        match v {
            Some(n) => self.uint(key, n),
            None => self.null(key),
        }
    }

    pub(crate) fn opt_int(&mut self, key: Option<&str>, v: Option<i64>) {
        match v {
            Some(n) => self.int(key, n),
            None => self.null(key),
        }
    }

    pub(crate) fn opt_lsn(&mut self, key: Option<&str>, v: Option<u64>) {
        match v {
            Some(l) => self.str(key, &lsn_text(l)),
            None => self.null(key),
        }
    }

    pub(crate) fn bool(&mut self, key: Option<&str>, v: bool) {
        self.key(key);
        self.buf.push_str(if v { "true" } else { "false" });
    }

    pub(crate) fn null(&mut self, key: Option<&str>) {
        self.key(key);
        self.buf.push_str("null");
    }

    pub(crate) fn finish(self) -> String {
        self.buf
    }
}

fn push_str_lit(buf: &mut String, s: &str) {
    buf.push('"');
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(buf, "\\u{:04x}", c as u32);
            }
            c => buf.push(c),
        }
    }
    buf.push('"');
}

pub(crate) fn error_body(code: &str, message: &str) -> String {
    let mut j = JsonOut::new();
    j.open(None);
    j.str(Some("error"), code);
    j.str(Some("message"), message);
    j.close();
    j.finish()
}
