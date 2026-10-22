//! BKI reader (bootscanner.l + bootparse.y as one hand-written pass).
//!
//! Grammar (bootparse.y): open ID | close ID | create ID oid [bootstrap]
//! [shared_relation] [rowtype_oid oid] ( col type [FORCE NOT NULL|FORCE
//! NULL] , ... ) | insert ( value ... ) | declare [unique] index ID oid on
//! ID using ID ( col opclass , ... ) | declare toast oid oid on ID | build
//! indices. Tokens: `[-A-Za-z0-9_]+`, single-quoted strings with doubled
//! quotes, `_null_`, and the punctuation `, = ( )`; `#` comments to end of
//! line. The scanner's keyword tokens are ordinary identifiers here and are
//! recognized by position, exactly as bison's grammar would.

use types_error::{PgError, PgResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Id(String),
    Null,
    Comma,
    Equals,
    LParen,
    RParen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nullness {
    Auto,
    ForceNotNull,
    ForceNull,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub typ: String,
    pub nullness: Nullness,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Open(String),
    Close(String),
    Create {
        name: String,
        oid: u32,
        bootstrap: bool,
        shared: bool,
        rowtype_oid: u32,
        columns: Vec<Column>,
    },
    Insert(Vec<Option<String>>),
    DeclareIndex {
        unique: bool,
        name: String,
        oid: u32,
        on: String,
        using: String,
        params: Vec<(String, String)>,
    },
    DeclareToast {
        toast_oid: u32,
        index_oid: u32,
        on: String,
    },
    BuildIndices,
}

fn syntax(line: usize, msg: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(format!("{} at line {line}", msg.into())))
}

pub fn lex(src: &str) -> PgResult<Vec<(Tok, usize)>> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line = 1;
    let mut at_line_start = true;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                line += 1;
                i += 1;
                at_line_start = true;
                continue;
            }
            b'\r' | b'\t' | b' ' => {
                i += 1;
                continue;
            }
            b'#' if at_line_start => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b',' => toks.push((Tok::Comma, line)),
            b'=' => toks.push((Tok::Equals, line)),
            b'(' => toks.push((Tok::LParen, line)),
            b')' => toks.push((Tok::RParen, line)),
            b'\'' => {
                let mut s = Vec::new();
                i += 1;
                loop {
                    if i >= b.len() {
                        return Err(syntax(line, "unterminated quoted string"));
                    }
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            s.push(b'\'');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                toks.push((Tok::Id(String::from_utf8_lossy(&s).into_owned()), line));
                at_line_start = false;
                i += 1;
                continue;
            }
            _ if c == b'-' || c == b'_' || c.is_ascii_alphanumeric() => {
                let start = i;
                while i < b.len() && (b[i] == b'-' || b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                let word = &src[start..i];
                toks.push((if word == "_null_" { Tok::Null } else { Tok::Id(word.to_string()) }, line));
                at_line_start = false;
                continue;
            }
            _ => {
                return Err(syntax(
                    line,
                    format!("syntax error: unexpected character \"{}\"", c as char),
                ));
            }
        }
        at_line_start = false;
        i += 1;
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }
    fn line(&self) -> usize {
        self.toks.get(self.pos).or(self.toks.last()).map_or(0, |t| t.1)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        self.pos += 1;
        t
    }
    fn ident(&mut self) -> PgResult<String> {
        match self.next() {
            Some(Tok::Id(s)) => Ok(s),
            other => Err(syntax(self.line(), format!("syntax error: expected identifier, got {other:?}"))),
        }
    }
    fn keyword(&mut self, kw: &str) -> PgResult<()> {
        match self.next() {
            Some(Tok::Id(s)) if s == kw => Ok(()),
            other => Err(syntax(self.line(), format!("syntax error: expected \"{kw}\", got {other:?}"))),
        }
    }
    fn peek_is(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Id(s)) if s == kw)
    }
    fn expect(&mut self, t: Tok) -> PgResult<()> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            other => Err(syntax(self.line(), format!("syntax error: expected {t:?}, got {other:?}"))),
        }
    }
    fn oid(&mut self) -> PgResult<u32> {
        let s = self.ident()?;
        s.parse::<u32>().map_err(|_| syntax(self.line(), format!("syntax error: bad OID \"{s}\"")))
    }
}

pub fn parse(src: &str) -> PgResult<Vec<Stmt>> {
    let mut p = Parser { toks: lex(src)?, pos: 0 };
    let mut out = Vec::new();
    while let Some(tok) = p.peek() {
        let Tok::Id(kw) = tok else {
            return Err(syntax(p.line(), format!("syntax error: unexpected {tok:?}")));
        };
        let kw = kw.clone();
        p.pos += 1;
        match kw.as_str() {
            "open" => out.push(Stmt::Open(p.ident()?)),
            "close" => out.push(Stmt::Close(p.ident()?)),
            "create" => {
                let name = p.ident()?;
                let oid = p.oid()?;
                let mut bootstrap = false;
                let mut shared = false;
                let mut rowtype_oid = 0;
                if p.peek_is("bootstrap") {
                    p.pos += 1;
                    bootstrap = true;
                }
                if p.peek_is("shared_relation") {
                    p.pos += 1;
                    shared = true;
                }
                if p.peek_is("rowtype_oid") {
                    p.pos += 1;
                    rowtype_oid = p.oid()?;
                }
                p.expect(Tok::LParen)?;
                let mut columns = Vec::new();
                loop {
                    let cname = p.ident()?;
                    p.expect(Tok::Equals)?;
                    let typ = p.ident()?;
                    let mut nullness = Nullness::Auto;
                    if p.peek_is("FORCE") {
                        p.pos += 1;
                        if p.peek_is("NOT") {
                            p.pos += 1;
                            p.keyword("NULL")?;
                            nullness = Nullness::ForceNotNull;
                        } else {
                            p.keyword("NULL")?;
                            nullness = Nullness::ForceNull;
                        }
                    }
                    columns.push(Column { name: cname, typ, nullness });
                    match p.next() {
                        Some(Tok::Comma) => continue,
                        Some(Tok::RParen) => break,
                        other => return Err(syntax(p.line(), format!("syntax error in column list: {other:?}"))),
                    }
                }
                out.push(Stmt::Create { name, oid, bootstrap, shared, rowtype_oid, columns });
            }
            "insert" => {
                p.expect(Tok::LParen)?;
                let mut vals = Vec::new();
                loop {
                    match p.next() {
                        Some(Tok::Id(s)) => vals.push(Some(s)),
                        Some(Tok::Null) => vals.push(None),
                        Some(Tok::RParen) => break,
                        other => return Err(syntax(p.line(), format!("syntax error in insert: {other:?}"))),
                    }
                }
                out.push(Stmt::Insert(vals));
            }
            "declare" => {
                if p.peek_is("toast") {
                    p.pos += 1;
                    let toast_oid = p.oid()?;
                    let index_oid = p.oid()?;
                    p.keyword("on")?;
                    let on = p.ident()?;
                    out.push(Stmt::DeclareToast { toast_oid, index_oid, on });
                } else {
                    let unique = if p.peek_is("unique") {
                        p.pos += 1;
                        true
                    } else {
                        false
                    };
                    p.keyword("index")?;
                    let name = p.ident()?;
                    let oid = p.oid()?;
                    p.keyword("on")?;
                    let on = p.ident()?;
                    p.keyword("using")?;
                    let using = p.ident()?;
                    p.expect(Tok::LParen)?;
                    let mut params = Vec::new();
                    loop {
                        let col = p.ident()?;
                        let opclass = p.ident()?;
                        params.push((col, opclass));
                        match p.next() {
                            Some(Tok::Comma) => continue,
                            Some(Tok::RParen) => break,
                            other => return Err(syntax(p.line(), format!("syntax error in index params: {other:?}"))),
                        }
                    }
                    out.push(Stmt::DeclareIndex { unique, name, oid, on, using, params });
                }
            }
            "build" => {
                p.keyword("indices")?;
                out.push(Stmt::BuildIndices);
            }
            other => return Err(syntax(p.line(), format!("syntax error: unexpected \"{other}\""))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_statement_forms() {
        let src = "# PostgreSQL 18\ncreate pg_proc 1255 bootstrap rowtype_oid 81\n (\n oid = oid ,\n proname = name ,\n proargtypes = oidvector FORCE NOT NULL ,\n proacl = _aclitem\n )\ninsert ( 1242 boolin 'a b' _null_ )\nclose pg_proc\nopen pg_type\ncreate pg_authid 1260 shared_relation rowtype_oid 2842\n (\n oid = oid\n )\ndeclare unique index pg_proc_oid_index 2690 on pg_proc using btree(oid oid_ops)\ndeclare index pg_x 1 on pg_y using btree(a b, c d)\ndeclare toast 2836 2837 on pg_proc\nbuild indices\n";
        let stmts = parse(src).unwrap();
        assert_eq!(stmts.len(), 9);
        match &stmts[0] {
            Stmt::Create { name, oid, bootstrap, shared, rowtype_oid, columns } => {
                assert_eq!((name.as_str(), *oid, *bootstrap, *shared, *rowtype_oid), ("pg_proc", 1255, true, false, 81));
                assert_eq!(columns.len(), 4);
                assert_eq!(columns[2].nullness, Nullness::ForceNotNull);
            }
            s => panic!("{s:?}"),
        }
        match &stmts[1] {
            Stmt::Insert(v) => assert_eq!(v, &vec![Some("1242".into()), Some("boolin".into()), Some("a b".into()), None]),
            s => panic!("{s:?}"),
        }
        match &stmts[4] {
            Stmt::Create { shared, .. } => assert!(shared),
            s => panic!("{s:?}"),
        }
        match &stmts[6] {
            Stmt::DeclareIndex { unique, params, .. } => {
                assert!(!unique);
                assert_eq!(params.len(), 2);
            }
            s => panic!("{s:?}"),
        }
        assert!(matches!(stmts[8], Stmt::BuildIndices));
    }

    #[test]
    fn quoted_strings_unescape_and_keep_spaces() {
        let toks = lex("insert ( 'it''s' 'two words' -1 )").unwrap();
        assert_eq!(toks[2].0, Tok::Id("it's".into()));
        assert_eq!(toks[3].0, Tok::Id("two words".into()));
        assert_eq!(toks[4].0, Tok::Id("-1".into()));
    }

    #[test]
    fn comment_lines_are_dropped() {
        let toks = lex("# header\nopen x\n").unwrap();
        assert_eq!(toks.len(), 2);
    }
}
