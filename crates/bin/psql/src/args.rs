//! psql's command line, read the way startup.c:536 reads it:
//! `getopt_long(argc, argv, "aAbc:d:eEf:F:h:HlL:no:p:P:qR:sStT:U:v:VwWxXz?01", long_options, ...)`
//! with glibc's GNU permutation (an option after an operand is still an option).
//!
//! - short options bundle: `-At` = `-A -t`;
//! - a short option that takes an argument ends the bundle; its argument is the rest of the
//!   word (`-Atcselect 1`) or, when the word ends there, the next word (`-Atc 'select 1'`);
//! - `--name=value` or `--name value` for a long option that takes an argument;
//! - `--` ends the options: every later word is an operand; a lone `-` is an operand.
//!
//! Each option comes back under one spelling: `-X` for a short option, `--name` for a long one
//! (the spellings main.rs matches). Options C psql does not know fail here with glibc's text;
//! options C knows but this port does not implement are main.rs's to refuse.

/// startup.c:536's optstring: the short options C psql knows, `:` after one that takes an argument.
const OPTSTRING: &str = "aAbc:d:eEf:F:h:HlL:no:p:P:qR:sStT:U:v:VwWxXz?01";

/// startup.c:490 long_options (name, takes an argument), plus this port's `--fd-wire`.
/// `--help` is optional_argument in C: only `--help=VALUE` gives it one.
const LONG_OPTIONS: &[(&str, bool)] = &[
    ("echo-all", false),
    ("no-align", false),
    ("command", true),
    ("dbname", true),
    ("echo-queries", false),
    ("echo-errors", false),
    ("echo-hidden", false),
    ("file", true),
    ("field-separator", true),
    ("field-separator-zero", false),
    ("host", true),
    ("html", false),
    ("list", false),
    ("log-file", true),
    ("no-readline", false),
    ("single-transaction", false),
    ("output", true),
    ("port", true),
    ("pset", true),
    ("quiet", false),
    ("record-separator", true),
    ("record-separator-zero", false),
    ("single-step", false),
    ("single-line", false),
    ("tuples-only", false),
    ("table-attr", true),
    ("username", true),
    ("set", true),
    ("variable", true),
    ("version", false),
    ("no-password", false),
    ("password", false),
    ("expanded", false),
    ("no-psqlrc", false),
    ("help", false),
    ("csv", false),
    ("fd-wire", true),
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Arg {
    /// `-X` or `--name`, with its argument when it takes one (or `--name=value` gave one).
    Opt(String, Option<String>),
    Operand(String),
}

/// Some(true) = takes an argument, Some(false) = a flag, None = not in the optstring.
fn short_takes_arg(c: char) -> Option<bool> {
    if c == ':' {
        return None;
    }
    let i = OPTSTRING.find(c)?;
    Some(OPTSTRING[i + c.len_utf8()..].starts_with(':'))
}

/// Splits argv (without argv[0]) into options and operands. Err = glibc's message for an
/// option C psql does not know or an argument that is missing.
pub(crate) fn getopt_long(argv: &[String]) -> Result<Vec<Arg>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            out.extend(argv[i..].iter().cloned().map(Arg::Operand));
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let Some(&(_, takes)) = LONG_OPTIONS.iter().find(|(n, _)| *n == name) else {
                return Err(format!("unrecognized option '{a}'"));
            };
            let value = if takes {
                match inline {
                    Some(v) => Some(v),
                    None if i < argv.len() => {
                        i += 1;
                        Some(argv[i - 1].clone())
                    }
                    None => return Err(format!("option '--{name}' requires an argument")),
                }
            } else if inline.is_some() && name != "help" {
                return Err(format!("option '--{name}' doesn't allow an argument"));
            } else {
                inline
            };
            out.push(Arg::Opt(format!("--{name}"), value));
            continue;
        }
        if a.len() < 2 || !a.starts_with('-') {
            out.push(Arg::Operand(a.clone()));
            continue;
        }
        let body = &a[1..];
        for (k, c) in body.char_indices() {
            match short_takes_arg(c) {
                None => return Err(format!("invalid option -- '{c}'")),
                Some(false) => out.push(Arg::Opt(format!("-{c}"), None)),
                Some(true) => {
                    let rest = &body[k + c.len_utf8()..];
                    let value = if !rest.is_empty() {
                        rest.to_string()
                    } else if i < argv.len() {
                        i += 1;
                        argv[i - 1].clone()
                    } else {
                        return Err(format!("option requires an argument -- '{c}'"));
                    };
                    out.push(Arg::Opt(format!("-{c}"), Some(value)));
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{getopt_long, Arg};

    fn run(v: &[&str]) -> Result<Vec<Arg>, String> {
        getopt_long(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    fn o(n: &str, v: Option<&str>) -> Arg {
        Arg::Opt(n.to_string(), v.map(str::to_string))
    }
    fn p(s: &str) -> Arg {
        Arg::Operand(s.to_string())
    }

    #[test]
    fn bundled_flags_split() {
        assert_eq!(run(&["-At"]), Ok(vec![o("-A", None), o("-t", None)]));
        assert_eq!(run(&["-qXAtx"]).unwrap().len(), 5);
    }

    #[test]
    fn bundle_ending_in_an_argument_option_takes_the_next_word() {
        assert_eq!(
            run(&["-Atc", "select 1"]),
            Ok(vec![o("-A", None), o("-t", None), o("-c", Some("select 1"))])
        );
    }

    #[test]
    fn bundle_ending_in_an_argument_option_takes_the_rest_of_the_word() {
        assert_eq!(
            run(&["-Atcselect 1", "db"]),
            Ok(vec![o("-A", None), o("-t", None), o("-c", Some("select 1")), p("db")])
        );
        // the rest of the word is the argument even when it reads like options
        assert_eq!(run(&["-cAt"]), Ok(vec![o("-c", Some("At"))]));
        assert_eq!(run(&["-p5433"]), Ok(vec![o("-p", Some("5433"))]));
    }

    #[test]
    fn the_next_word_is_taken_even_when_it_starts_with_a_dash() {
        assert_eq!(run(&["-c", "-t"]), Ok(vec![o("-c", Some("-t"))]));
        assert_eq!(run(&["--command", "--"]), Ok(vec![o("--command", Some("--"))]));
    }

    #[test]
    fn long_options_with_and_without_equals() {
        assert_eq!(
            run(&["--command", "select 1", "--tuples-only", "--port=5433"]),
            Ok(vec![o("--command", Some("select 1")), o("--tuples-only", None), o("--port", Some("5433"))])
        );
        assert_eq!(run(&["--set=a=b"]), Ok(vec![o("--set", Some("a=b"))]));
        assert_eq!(run(&["--help"]), Ok(vec![o("--help", None)]));
        assert_eq!(run(&["--help=commands"]), Ok(vec![o("--help", Some("commands"))]));
    }

    #[test]
    fn double_dash_ends_options_and_operands_permute() {
        assert_eq!(
            run(&["db", "-A", "--", "-t", "--x"]),
            Ok(vec![p("db"), o("-A", None), p("-t"), p("--x")])
        );
        assert_eq!(run(&["-", "-X"]), Ok(vec![p("-"), o("-X", None)]));
    }

    #[test]
    fn errors_name_the_option_as_glibc_does() {
        assert_eq!(run(&["-AZ"]), Err("invalid option -- 'Z'".to_string()));
        assert_eq!(run(&["-At", "--bogus"]), Err("unrecognized option '--bogus'".to_string()));
        assert_eq!(run(&["-Atc"]), Err("option requires an argument -- 'c'".to_string()));
        assert_eq!(run(&["--command"]), Err("option '--command' requires an argument".to_string()));
        assert_eq!(run(&["--quiet=1"]), Err("option '--quiet' doesn't allow an argument".to_string()));
        assert_eq!(run(&["-:"]), Err("invalid option -- ':'".to_string()));
    }

    #[test]
    fn options_c_knows_come_back_for_main_to_judge() {
        // -a / --csv are C psql options this port does not implement: main.rs refuses them.
        assert_eq!(run(&["-aA"]), Ok(vec![o("-a", None), o("-A", None)]));
        assert_eq!(run(&["--csv"]), Ok(vec![o("--csv", None)]));
        assert_eq!(run(&["-?"]), Ok(vec![o("-?", None)]));
    }
}
