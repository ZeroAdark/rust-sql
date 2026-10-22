//! initdb.c's text-file helpers: readfile/writefile, replace_token,
//! replace_guc_value (with C's comment re-alignment), and the quoting
//! rules (escape_quotes, escape_quotes_bki, guc_value_requires_quotes).

pub fn readfile(path: &str) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not open file \"{path}\" for reading: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // Lines keep their trailing newline, as C's readfile does.
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            lines.push(text[start..=i].to_string());
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    Ok(lines)
}

pub fn writefile(path: &str, lines: &[String]) -> Result<(), String> {
    let mut out = String::new();
    for l in lines {
        out.push_str(l);
    }
    std::fs::write(path, out).map_err(|e| format!("could not write file \"{path}\": {e}"))
}

/// `replace_token`: plain substring replacement on every line.
pub fn replace_token(lines: Vec<String>, token: &str, replacement: &str) -> Vec<String> {
    lines.into_iter().map(|l| l.replace(token, replacement)).collect()
}

/// `escape_quotes` (escape_single_quotes_ascii): ' -> ''.
pub fn escape_quotes(s: &str) -> String {
    s.replace('\'', "''")
}

/// `escape_quotes_bki`: escape_quotes, then quote iff the value has
/// characters the BKI scanner would not take as one identifier.
pub fn escape_quotes_bki(s: &str) -> String {
    let data = escape_quotes(s);
    let needs_quotes = data.is_empty()
        || !data.bytes().all(|c| c == b'-' || c == b'_' || c.is_ascii_alphanumeric());
    if needs_quotes { format!("'{data}'") } else { data }
}

/// `guc_value_requires_quotes`.
pub fn guc_value_requires_quotes(v: &str) -> bool {
    let b = v.as_bytes();
    if b.is_empty() {
        return true;
    }
    if b[0].is_ascii_alphabetic() {
        return !b.iter().all(|c| c.is_ascii_alphanumeric());
    }
    if b[0].is_ascii_digit() {
        let rest = &b[b.iter().take_while(|c| c.is_ascii_digit()).count()..];
        return !rest.iter().all(|c| c.is_ascii_alphabetic());
    }
    true
}

/// `replace_guc_value`: rewrite the (first) line that sets `name` in the
/// sample, keeping its trailing comment aligned; append a new line if the
/// sample has none.
pub fn replace_guc_value(mut lines: Vec<String>, name: &str, value: &str, mark_as_comment: bool) -> Vec<String> {
    let mut newline = String::new();
    if mark_as_comment {
        newline.push('#');
    }
    newline.push_str(name);
    newline.push_str(" = ");
    if guc_value_requires_quotes(value) {
        newline.push('\'');
        newline.push_str(&escape_quotes(value));
        newline.push('\'');
    } else {
        newline.push_str(value);
    }
    let namelen = name.len();
    for i in 0..lines.len() {
        let line = &lines[i];
        let b = line.as_bytes();
        let mut where_ = 0;
        while where_ < b.len() && (b[where_] == b'#' || b[where_].is_ascii_whitespace()) {
            where_ += 1;
        }
        if where_ + namelen > b.len() || !line[where_..where_ + namelen].eq_ignore_ascii_case(name) {
            continue;
        }
        let namestart = where_;
        where_ += namelen;
        while where_ < b.len() && b[where_].is_ascii_whitespace() {
            where_ += 1;
        }
        if where_ >= b.len() || b[where_] != b'=' {
            continue;
        }
        // Use the canonical casing shown in the file.
        let off = if mark_as_comment { 1 } else { 0 };
        newline.replace_range(off..off + namelen, &line[namestart..namestart + namelen]);
        if let Some(hash) = line[where_..].rfind('#').map(|p| p + where_) {
            let mut oldindent = 0usize;
            for &c in &b[..hash] {
                if c == b'\t' { oldindent += 8 - (oldindent % 8); } else { oldindent += 1; }
            }
            let mut newindent = newline.len();
            let oldindent = oldindent.max(newindent + 1);
            while newindent < oldindent {
                let if_tab = newindent + 8 - (newindent % 8);
                if if_tab <= oldindent {
                    newline.push('\t');
                    newindent = if_tab;
                } else {
                    newline.push(' ');
                    newindent += 1;
                }
            }
            newline.push_str(&line[hash..]);
        } else {
            newline.push('\n');
        }
        lines[i] = newline;
        return lines;
    }
    newline.push('\n');
    lines.push(newline);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guc_quoting_rules() {
        assert!(!guc_value_requires_quotes("on"));
        assert!(!guc_value_requires_quotes("128MB"));
        assert!(!guc_value_requires_quotes("100"));
        assert!(guc_value_requires_quotes("iso, mdy"));
        assert!(guc_value_requires_quotes(""));
        assert!(guc_value_requires_quotes("/tmp"));
    }

    #[test]
    fn replace_keeps_comment_alignment_and_casing() {
        let lines = vec!["#Max_Connections = 100\t\t\t# (change requires restart)\n".to_string()];
        let out = replace_guc_value(lines, "max_connections", "50", false);
        assert_eq!(out[0], "Max_Connections = 50\t\t\t# (change requires restart)\n");
        let out = replace_guc_value(vec!["#port = 5432\n".to_string()], "port", "5432", true);
        assert_eq!(out[0], "#port = 5432\n");
        // Identifier-shaped values stay bare, path-shaped ones get quotes (C).
        let out = replace_guc_value(vec![], "timezone", "UTC", false);
        assert_eq!(out[0], "timezone = UTC\n");
        let out = replace_guc_value(vec![], "timezone", "America/Los_Angeles", false);
        assert_eq!(out[0], "timezone = 'America/Los_Angeles'\n");
    }

    #[test]
    fn bki_escaping() {
        assert_eq!(escape_quotes_bki("postgres"), "postgres");
        assert_eq!(escape_quotes_bki("a b"), "'a b'");
        assert_eq!(escape_quotes_bki("it's"), "'it''s'");
        assert_eq!(escape_quotes_bki("C.UTF-8"), "'C.UTF-8'");
    }
}
