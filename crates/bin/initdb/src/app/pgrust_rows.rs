const PGRUST_BKI: &str = include_str!("../pgrust.bki");

fn rows() -> Result<Vec<(&'static str, &'static str)>, String> {
    PGRUST_BKI
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_once(' ').ok_or_else(|| format!("malformed pgrust.bki row: {l}")))
        .collect()
}

/// The vendored postgres.bki with each pgrust.bki row inserted just before
/// its catalog's `close` line.
pub fn splice(lines: Vec<String>) -> Result<Vec<String>, String> {
    let rows = rows()?;
    let mut placed = vec![0usize; rows.len()];
    let mut out = Vec::with_capacity(lines.len() + rows.len());
    for line in lines {
        if let Some(cat) = line.strip_prefix("close ").map(str::trim_end) {
            for (i, (c, stmt)) in rows.iter().enumerate() {
                if *c == cat {
                    out.push(format!("{stmt}\n"));
                    placed[i] += 1;
                }
            }
        }
        out.push(line);
    }
    match placed.iter().position(|&n| n != 1) {
        Some(i) => Err(format!("pgrust.bki: postgres.bki must close {} exactly once", rows[i].0)),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vendored() -> Vec<String> {
        crate::app::conf::readfile(concat!(env!("CARGO_MANIFEST_DIR"), "/share/postgres.bki")).expect("vendored postgres.bki")
    }

    fn fields(stmt: &str) -> Vec<String> {
        let body = stmt.trim().strip_prefix("insert (").and_then(|s| s.strip_suffix(')')).expect("insert ( ... )");
        let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
        for ch in body.chars() {
            match ch {
                '\'' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        assert!(!quoted, "unterminated quote in {stmt}");
        out
    }

    fn block<'a>(bki: &'a [String], cat: &str) -> &'a [String] {
        let start = bki.iter().position(|l| l.starts_with(&format!("create {cat} "))).expect("create line");
        let end = bki.iter().position(|l| l.trim_end() == format!("close {cat}")).expect("close line");
        &bki[start..end]
    }

    fn key(cat: &str, f: &[String]) -> String {
        if cat == "pg_description" { format!("{} {}", f[0], f[1]) } else { f[0].clone() }
    }

    #[test]
    fn rows_join_their_catalog_block_with_full_arity_and_fresh_keys() {
        let base = vendored();
        let spliced = splice(base.clone()).expect("splice");
        let rows = rows().expect("rows");
        assert!(!rows.is_empty());
        assert_eq!(spliced.len(), base.len() + rows.len());
        for (cat, stmt) in &rows {
            let orig = block(&base, cat);
            let columns = orig
                .iter()
                .take_while(|l| !l.starts_with("insert") && !l.starts_with("open"))
                .filter(|l| l.contains(" = "))
                .count();
            let f = fields(stmt);
            assert_eq!(f.len(), columns, "{cat}: {stmt}");
            let taken = orig.iter().filter(|l| l.starts_with("insert")).any(|l| key(cat, &fields(l)) == key(cat, &f));
            assert!(!taken, "{cat} already has a row keyed {}", key(cat, &f));
            let blk = block(&spliced, cat);
            let at = blk.iter().position(|l| l.trim_end() == *stmt).expect("row inside its catalog block");
            assert!(blk[at..].iter().all(|l| l.starts_with("insert")), "{cat}: rows sit right before close");
        }
    }
}
