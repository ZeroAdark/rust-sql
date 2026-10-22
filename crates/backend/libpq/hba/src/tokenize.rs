use std::cell::Cell;
use std::io::BufRead;
use std::path::PathBuf;
use std::rc::Rc;

use types_error::{ErrorLevel, PgResult};
use types_startup::AuthToken;

use crate::token::{free_auth_file, make_auth_token, next_token, open_auth_file, FileHandle};
use crate::{report_log, TokenizedAuthLine, CONF_FILE_START_DEPTH};
use elog::PCT_M;

// errno after a failed open_auth_file (C reads the ambient errno for the
// include_if_exists ENOENT test).
thread_local! {
    static LAST_OPEN_ERRNO: Cell<i32> = const { Cell::new(0) };
}

pub(crate) fn set_last_open_errno(errno: i32) {
    LAST_OPEN_ERRNO.with(|c| c.set(errno));
}

fn absolute_config_location(inc_filename: &str, outer_filename: &str) -> String {
    conffiles_seams::absolute_config_location::call(
        inc_filename.to_string(),
        Some(PathBuf::from(outer_filename)),
    )
    .to_string_lossy()
    .into_owned()
}

pub(crate) fn next_field_expand(
    filename: &str,
    line: &[u8],
    pos: &mut usize,
    elevel: ErrorLevel,
    depth: i32,
    err_msg: &mut Option<String>,
    tok_lines: &mut Vec<TokenizedAuthLine>,
) -> PgResult<Vec<AuthToken>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut trailing_comma = false;
    let mut initial_quote = false;
    let mut tokens: Vec<AuthToken> = Vec::new();

    loop {
        if !next_token(line, pos, &mut buf, &mut initial_quote, &mut trailing_comma) {
            break;
        }

        // Is this referencing a file?
        if !initial_quote && buf.len() > 1 && buf[0] == b'@' {
            let inc = String::from_utf8_lossy(&buf[1..]).into_owned();
            tokenize_expand_file(&mut tokens, filename, &inc, elevel, depth + 1, err_msg, tok_lines)?;
        } else {
            tokens.push(make_auth_token(&buf, initial_quote)?);
        }

        if !(trailing_comma && err_msg.is_none()) {
            break;
        }
    }

    Ok(tokens)
}

pub(crate) fn tokenize_include_file(
    outer_filename: &str,
    inc_filename: &str,
    tok_lines: &mut Vec<TokenizedAuthLine>,
    elevel: ErrorLevel,
    depth: i32,
    missing_ok: bool,
    err_msg: &mut Option<String>,
) -> PgResult<()> {
    let inc_fullname = absolute_config_location(inc_filename, outer_filename);
    let inc_file = match open_auth_file(&inc_fullname, elevel, depth, err_msg)? {
        None => {
            if LAST_OPEN_ERRNO.with(Cell::get) == libc::ENOENT && missing_ok {
                report_log(
                    elevel,
                    479,
                    "tokenize_include_file",
                    format!("skipping missing authentication file \"{inc_fullname}\""),
                )?;
                *err_msg = None;
                return Ok(());
            }
            // error in err_msg, so caller reports it.
            return Ok(());
        }
        Some(file) => file,
    };

    tokenize_auth_file(&inc_fullname, &inc_file, tok_lines, elevel, depth)?;
    free_auth_file(inc_file, depth);
    Ok(())
}

pub(crate) fn tokenize_expand_file(
    tokens: &mut Vec<AuthToken>,
    outer_filename: &str,
    inc_filename: &str,
    elevel: ErrorLevel,
    depth: i32,
    err_msg: &mut Option<String>,
    _tok_lines: &mut [TokenizedAuthLine],
) -> PgResult<()> {
    let inc_fullname = absolute_config_location(inc_filename, outer_filename);
    let Some(inc_file) = open_auth_file(&inc_fullname, elevel, depth, err_msg)? else {
        return Ok(()); // error already logged
    };

    let mut inc_lines: Vec<TokenizedAuthLine> = Vec::new();
    tokenize_auth_file(&inc_fullname, &inc_file, &mut inc_lines, elevel, depth)?;

    for tok_line in inc_lines {
        if let Some(e) = &tok_line.err_msg {
            *err_msg = Some(e.clone());
            break;
        }
        for inc_tokens in tok_line.fields {
            tokens.extend(inc_tokens);
        }
    }

    free_auth_file(inc_file, depth);
    Ok(())
}

// tokenize_error_callback (hba.c:662): while tokenize_auth_file is on the
// stack every report carries 'line %d of configuration file "%s"' CONTEXT.
// The callback reads the live line number (C: callback_arg.linenum follows
// line_number); nested include frames push their own, innermost first, and
// the guard pops on every exit (C: error_context_stack = previous).
struct TokenizeErrorContext {
    linenum: Rc<Cell<i32>>,
    callback: u64,
}

impl TokenizeErrorContext {
    fn push(filename: &str, line_number: i32) -> Self {
        let linenum = Rc::new(Cell::new(line_number));
        let callback = {
            let linenum = Rc::clone(&linenum);
            let filename = filename.to_owned();
            elog::push_context_frame(Box::new(move |e| {
                e.add_context_line(crate::line_context(linenum.get(), &filename));
            }), true)
        };
        TokenizeErrorContext { linenum, callback }
    }
}

impl Drop for TokenizeErrorContext {
    fn drop(&mut self) {
        elog::pop_emit_context_callback(self.callback);
    }
}

pub fn tokenize_auth_file<R: BufRead>(
    filename: &str,
    file: &FileHandle<R>,
    tok_lines: &mut Vec<TokenizedAuthLine>,
    elevel: ErrorLevel,
    depth: i32,
) -> PgResult<()> {
    let mut reader = file.reader.borrow_mut();
    let mut lines = LineReader::new(&mut *reader);
    let mut buf = Vec::new();
    let mut line_number: i32 = 1;
    let errcontext = TokenizeErrorContext::push(filename, line_number);

    if depth == CONF_FILE_START_DEPTH {
        tok_lines.clear();
    }

    while !lines.done {
        let mut current_line: Vec<Vec<AuthToken>> = Vec::new();
        let mut err_msg: Option<String> = None;
        let mut last_backslash_buflen: usize = 0;
        let mut continuations: i32 = 0;

        // Collect the next input line, handling backslash continuations.
        buf.clear();
        while lines.append(&mut buf)? {
            while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                buf.pop();
            }

            if buf.len() > last_backslash_buflen && buf.last() == Some(&b'\\') {
                buf.pop();
                last_backslash_buflen = buf.len();
                continuations += 1;
                continue;
            }
            break;
        }

        if lines.read_errno.is_some() {
            break;
        }

        let mut lineptr: usize = 0;
        while lineptr < buf.len() && err_msg.is_none() {
            let current_field =
                next_field_expand(filename, &buf, &mut lineptr, elevel, depth, &mut err_msg, tok_lines)?;
            if !current_field.is_empty() {
                current_line.push(current_field);
            }
        }

        // The C body's process_line / next_line goto labels.
        let mut goto_next_line = current_line.is_empty() && err_msg.is_none();

        if !goto_next_line && err_msg.is_none() && current_line.len() == 2 {
            let first = current_line[0][0].string.clone();
            let second = current_line[1][0].string.clone();

            if first == "include" {
                tokenize_include_file(filename, &second, tok_lines, elevel, depth + 1, false, &mut err_msg)?;
                if err_msg.is_none() {
                    goto_next_line = true;
                }
            } else if first == "include_dir" {
                let res = conffiles_seams::get_conf_files_in_dir::call(
                    second.clone(),
                    Some(PathBuf::from(filename)),
                    elevel,
                )?;
                if let Some(m) = res.err_msg {
                    err_msg = Some(m);
                } else {
                    let mut err_buf = String::new();
                    for fname in &res.filenames {
                        let fname_s = fname.to_string_lossy().into_owned();
                        tokenize_include_file(
                            filename, &fname_s, tok_lines, elevel, depth + 1, false, &mut err_msg,
                        )?;
                        if let Some(e) = &err_msg {
                            if !err_buf.is_empty() {
                                err_buf.push('\n');
                            }
                            err_buf.push_str(e);
                        }
                    }
                    if err_buf.is_empty() {
                        goto_next_line = true;
                    } else {
                        err_msg = Some(err_buf);
                    }
                }
            } else if first == "include_if_exists" {
                tokenize_include_file(filename, &second, tok_lines, elevel, depth + 1, true, &mut err_msg)?;
                if err_msg.is_none() {
                    goto_next_line = true;
                }
            }
        }

        if !goto_next_line {
            tok_lines.push(TokenizedAuthLine {
                fields: current_line,
                file_name: filename.to_string(),
                line_num: line_number,
                raw_line: String::from_utf8_lossy(&buf).into_owned(),
                err_msg: err_msg.take(),
            });
        }

        line_number += continuations + 1;
        errcontext.linenum.set(line_number);
    }

    if let Some(errno) = lines.read_errno {
        elog::ereport(elevel)
            .with_saved_errno(errno)
            .errcode_for_file_access()
            .errmsg(format!("could not read file \"{filename}\": {PCT_M}"))
            .finish(crate::loc(769, "tokenize_auth_file"))?;
    }

    Ok(())
}

struct LineReader<R> {
    reader: R,
    maxlen: usize,
    done: bool,
    read_errno: Option<i32>,
}

impl<R: BufRead> LineReader<R> {
    fn new(reader: R) -> Self {
        Self { reader, maxlen: 1024, done: false, read_errno: None }
    }

    fn append(&mut self, buf: &mut Vec<u8>) -> PgResult<bool> {
        let orig_len = buf.len();
        while !self.done {
            mcx::std_vec_reserve("hba", buf, self.maxlen - buf.len())?;
            let mut remaining = self.maxlen - buf.len() - 1;
            let mut read_any = false;
            let mut nul = false;
            while remaining > 0 {
                let bytes = match self.reader.fill_buf() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        self.read_errno = Some(e.raw_os_error().unwrap_or(0));
                        self.done = true;
                        buf.truncate(orig_len);
                        return Ok(false);
                    }
                };
                if bytes.is_empty() {
                    self.done = true;
                    break;
                }
                let bytes = &bytes[..bytes.len().min(remaining)];
                let end = bytes.iter().position(|&b| b == b'\n').map_or(bytes.len(), |i| i + 1);
                let newline = bytes[end - 1] == b'\n';
                if !nul {
                    let len = bytes[..end].iter().position(|&b| b == 0).unwrap_or(end);
                    buf.extend_from_slice(&bytes[..len]);
                    nul = len < end;
                }
                self.reader.consume(end);
                remaining -= end;
                read_any = true;
                if newline {
                    break;
                }
            }
            if !read_any {
                break;
            }
            if buf.len() > orig_len && buf.last() == Some(&b'\n') {
                return Ok(true);
            }
            self.maxlen = line_capacity(buf.len(), self.maxlen)?;
        }
        Ok(buf.len() > orig_len)
    }
}

fn line_capacity(len: usize, maxlen: usize) -> PgResult<usize> {
    const LIMIT: usize = mcx::MAX_ALLOC_SIZE;
    if 128 >= LIMIT.saturating_sub(len) {
        return line_limit_error(len);
    }
    let needed = len + 128 + 1;
    Ok(if needed <= maxlen { maxlen } else { (maxlen * 2).max(needed.next_power_of_two()).min(LIMIT) })
}

#[cold]
fn line_limit_error(len: usize) -> PgResult<usize> {
    let mut report = elog::ereport(types_error::ERROR)
        .errcode(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)
        .errmsg(format!("string buffer exceeds maximum allowed length ({} bytes)", mcx::MAX_ALLOC_SIZE))
        .errdetail(format!("Cannot enlarge string buffer containing {len} bytes by 128 more bytes."));
    if let Some(context) = elog::GetErrorContextStack() {
        report = report.errcontext_msg(context);
    }
    report.finish(types_error::ErrorLocation::new("stringinfo.c", 360, "enlargeStringInfo"))?;
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_growth_limit_has_c_diagnostics() {
        let limit = mcx::MAX_ALLOC_SIZE;
        assert_eq!(line_capacity(limit - 129, limit).unwrap(), limit);
        let _context = TokenizeErrorContext::push("auth.conf", 7);
        let err = line_capacity(limit - 128, limit).unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(err.message, "string buffer exceeds maximum allowed length (1073741823 bytes)");
        assert_eq!(err.detail.as_deref(), Some("Cannot enlarge string buffer containing 1073741695 bytes by 128 more bytes."));
        assert_eq!(err.context.as_deref(), Some("line 7 of configuration file \"auth.conf\""));
        assert_eq!(err.location, Some(types_error::ErrorLocation::new("stringinfo.c", 360, "enlargeStringInfo")));
    }

    #[test]
    fn incremental_lines_preserve_capacity_and_continuation_length() {
        let mut lines = LineReader::new(&b"abc\\\nxyz\nlast"[..]);
        lines.maxlen = 8;
        let mut buf = Vec::new();
        assert!(lines.append(&mut buf).unwrap());
        assert_eq!(buf, b"abc\\\n");
        assert_eq!(lines.maxlen, 8);
        buf.truncate(3);
        assert!(lines.append(&mut buf).unwrap());
        assert_eq!(buf, b"abcxyz\n");
        assert_eq!(lines.maxlen, 8);
        buf.clear();
        assert!(lines.append(&mut buf).unwrap());
        assert_eq!(buf, b"last");
        assert_eq!(lines.maxlen, 256);
        assert!(!lines.append(&mut buf).unwrap());

        let mut lines = LineReader::new(&b"abc\\\ndefgh\n"[..]);
        lines.maxlen = 8;
        buf.clear();
        assert!(lines.append(&mut buf).unwrap());
        buf.truncate(3);
        assert!(lines.append(&mut buf).unwrap());
        assert_eq!(buf, b"abcdefgh\n");
        assert_eq!(lines.maxlen, 256);
    }

    #[test]
    fn chunk_growth_and_many_short_lines() {
        let input = b"abcdefghijk\na\nb\nc\n";
        let mut lines = LineReader::new(std::io::BufReader::with_capacity(2, &input[..]));
        lines.maxlen = 8;
        let mut buf = Vec::new();
        assert!(lines.append(&mut buf).unwrap());
        assert_eq!(buf, b"abcdefghijk\n");
        assert_eq!(lines.maxlen, 256);
        for expected in [b"a\n", b"b\n", b"c\n"] {
            buf.clear();
            assert!(lines.append(&mut buf).unwrap());
            assert_eq!(buf, expected);
            assert_eq!(lines.maxlen, 256);
        }
        buf.clear();
        assert!(!lines.append(&mut buf).unwrap());
    }
}
