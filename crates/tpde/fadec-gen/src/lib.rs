// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

pub mod decode;
pub mod encode;
pub mod encode2;
pub mod model;
pub mod parser;

use std::fmt::Write;

pub type Result<T> = std::result::Result<T, String>;
pub const UPSTREAM_REVISION: &str = "3994d89500985491b1a7ccc17827a22058b3de49";
pub const INPUT_SHA256: &str = "bd8911845e6d9900e9dbcc75bf3f36cce9ca378460a0fca4fa8bb5a45d066819";
pub const INSTRUCTION_TABLE: &str = include_str!("../data/instrs.txt");
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Decode,
    Encode,
    Encode2,
}

impl Mode {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "decode" => Ok(Self::Decode),
            "encode" => Ok(Self::Encode),
            "encode2" => Ok(Self::Encode2),
            _ => Err(format!("unknown generator mode: {name}")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    pub modes: Vec<u8>,
    pub with_undoc: bool,
}

#[derive(Clone, Debug)]
pub struct Generated {
    pub public_rs: String,
    pub private_rs: String,
    pub semantic_tsv: String,
    pub forms_tsv: String,
    pub stats: String,
    pub source_rows: usize,
    pub admitted_rows: usize,
}

pub fn source_forms(input: &str, entries: &[model::Entry]) -> Result<String> {
    let lines: Vec<_> = parser::table_lines(input).collect();
    let mut output = String::new();
    for entry in entries {
        let line = lines
            .get(entry.line.saturating_sub(1))
            .ok_or("invalid source line")?;
        let opcode = line
            .strip_prefix('*')
            .unwrap_or(line)
            .split(model::is_whitespace)
            .find(|field| !field.is_empty())
            .ok_or("missing opcode")?;
        let operands = entry
            .desc
            .operands
            .iter()
            .map(|op| format!("{}{}", op.regkind, op.sizestr))
            .collect::<Vec<_>>()
            .join(",");
        let flags = entry
            .desc
            .flags
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",");
        writeln!(
            output,
            "SOURCE\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entry.line,
            u8::from(entry.weak),
            opcode,
            entry.desc.encoding,
            operands,
            entry.desc.mnemonic,
            flags,
            u8::from(!entry.desc.flags.contains("UNDOC")),
            u8::from(!entry.desc.flags.contains("O64")),
            u8::from(!entry.desc.flags.contains("I64")),
            u8::from(
                !entry.desc.flags.contains("I64") && !entry.desc.mnemonic.starts_with("RESERVED_")
            )
        )
        .unwrap();
    }
    Ok(output)
}

/// ```
/// use tpde_fadec_gen::{generate, Mode, Options};
/// let output = generate("90 NP - - - - NOP", &Options {
///     mode: Mode::Encode, modes: vec![], with_undoc: false,
/// }).unwrap();
/// assert!(output.public_rs.contains("pub const FE_NOP: u64 = 0x90;"));
/// assert_eq!((output.source_rows, output.admitted_rows), (1, 1));
/// ```
pub fn generate(input: &str, options: &Options) -> Result<Generated> {
    if options.modes.iter().any(|mode| !matches!(*mode, 32 | 64)) {
        return Err("architecture mode must be 32 or 64".into());
    }
    let all_entries = parser::parse_table(input, true)?;
    let forms_tsv = source_forms(input, &all_entries)?;
    let source_rows = all_entries.len();
    let entries: Vec<_> = all_entries
        .into_iter()
        .filter(|entry| options.with_undoc || !entry.desc.flags.contains("UNDOC"))
        .collect();
    let admitted_rows = entries.len();
    let (public_rs, private_rs, semantic_tsv, stats) = match options.mode {
        Mode::Decode => {
            let result = decode::generate(&entries, &options.modes)?;
            (
                result.public_rs,
                result.private_rs,
                result.semantic_tsv,
                result.stats,
            )
        }
        Mode::Encode => {
            let result = encode::generate(&entries)?;
            (
                result.public_rs,
                result.private_rs,
                result.semantic_tsv,
                String::new(),
            )
        }
        Mode::Encode2 => {
            let result = encode2::generate(&entries)?;
            (
                result.public_rs,
                result.private_rs,
                result.semantic_tsv,
                String::new(),
            )
        }
    };
    Ok(Generated {
        public_rs,
        private_rs,
        semantic_tsv,
        forms_tsv,
        stats,
        source_rows,
        admitted_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_source_identities_follow_python_line_boundaries() {
        let options = Options {
            mode: Mode::Encode,
            modes: vec![64],
            with_undoc: false,
        };
        let expected =
            generate("90 NP - - - - NOP\n*91 NP - - - - OTHER UNDOC\n", &options).unwrap();
        for separator in [
            "\n", "\r", "\r\n", "\u{b}", "\u{c}", "\u{1c}", "\u{1d}", "\u{1e}", "\u{85}",
            "\u{2028}", "\u{2029}",
        ] {
            let input =
                format!("90 NP - - - - NOP{separator}*91 NP - - - - OTHER UNDOC{separator}");
            let generated = generate(&input, &options).unwrap();
            assert_eq!(generated.forms_tsv, expected.forms_tsv, "{separator:?}");
            assert_eq!(
                generated.semantic_tsv, expected.semantic_tsv,
                "{separator:?}"
            );
            assert_eq!((generated.source_rows, generated.admitted_rows), (2, 1));
        }
    }

    #[test]
    fn source_roster_obeys_python_field_whitespace() {
        let options = Options {
            mode: Mode::Decode,
            modes: vec![64],
            with_undoc: true,
        };
        let expected = generate("90 NP - - - - NOP\n*91 NP - - - - OTHER", &options).unwrap();
        let input = "\u{1f}90\u{1f}NP\u{1f}- - - - NOP\n*\u{1f}91\u{1f}NP - - - - OTHER";
        let generated = generate(input, &options).unwrap();
        assert_eq!(generated.forms_tsv, expected.forms_tsv);
        assert_eq!(generated.semantic_tsv, expected.semantic_tsv);
    }

    #[test]
    fn generation_failure_preserves_caller_input_and_next_call() {
        for mode in [Mode::Decode, Mode::Encode, Mode::Encode2] {
            let options = Options {
                mode,
                modes: vec![64],
                with_undoc: false,
            };
            let valid = String::from("90 NP - - - - NOP");
            let before = generate(&valid, &options).unwrap();
            assert!(generate("NP.90 NP - - - - BAD UNDOC", &options).is_err());
            let after = generate(&valid, &options).unwrap();
            assert_eq!(valid, "90 NP - - - - NOP");
            assert_eq!(before.public_rs, after.public_rs);
            assert_eq!(before.private_rs, after.private_rs);
            assert_eq!(before.semantic_tsv, after.semantic_tsv);
        }
    }

    #[test]
    fn undocumented_unknown_sizes_are_evaluated_after_filtering() {
        let input = "90 M Ebogus - - - BAD UNDOC";
        assert_eq!(parser::parse_table(input, true).unwrap().len(), 1);
        assert!(parser::parse_table(input, false).unwrap().is_empty());
        for mode in [Mode::Decode, Mode::Encode, Mode::Encode2] {
            let mut options = Options {
                mode,
                modes: vec![64],
                with_undoc: false,
            };
            let output = generate(input, &options).unwrap();
            assert_eq!((output.source_rows, output.admitted_rows), (1, 0));
            options.with_undoc = true;
            assert!(generate(input, &options).is_err());
        }
        assert!(
            parser::parse_table("VEX.NP.LIG.0f10 RM Vbogus Wbogus - - BAD UNDOC", false).is_err()
        );
    }
}
