// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::io::{Read, Write};
use tpde_fadec_gen::{Mode, Options, Result};

const USAGE: &str = "fadec-gen [--32] [--64] [--with-undoc] [--stats] [--forms PATH] [--semantic PATH] {decode,encode,encode2} TABLE OUT_PUBLIC OUT_PRIVATE";

struct Command {
    options: Options,
    stats: bool,
    table: String,
    public: String,
    private: String,
    forms: Option<String>,
    semantic: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Command>> {
    let mut modes = Vec::new();
    let mut with_undoc = false;
    let mut stats = false;
    let mut forms = None;
    let mut semantic = None;
    let mut positional = Vec::new();
    let mut args = args.peekable();
    let mut end_options = false;
    while let Some(arg) = args.next() {
        if end_options {
            positional.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => end_options = true,
            "-h" | "--help" => return Ok(None),
            "--32" => modes.push(32),
            "--64" => modes.push(64),
            "--with-undoc" => with_undoc = true,
            "--stats" => stats = true,
            "--forms" => forms = Some(args.next().ok_or("--forms requires a path")?),
            "--semantic" => semantic = Some(args.next().ok_or("--semantic requires a path")?),
            value if value.starts_with('-') && value != "-" => {
                return Err(format!("unknown option: {value}"));
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 4 {
        return Err(format!("expected four positional arguments; {USAGE}"));
    }
    let mode = Mode::parse(&positional[0])?;
    if mode == Mode::Decode && modes.is_empty() {
        return Err("decode requires --32 or --64".into());
    }
    let mut positional = positional.into_iter();
    positional.next();
    Ok(Some(Command {
        options: Options {
            mode,
            modes,
            with_undoc,
        },
        stats,
        table: positional.next().unwrap(),
        public: positional.next().unwrap(),
        private: positional.next().unwrap(),
        forms,
        semantic,
    }))
}

fn read_input(path: &str) -> Result<String> {
    if path == "-" {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| error.to_string())?;
        Ok(input)
    } else {
        std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))
    }
}

fn write_output(path: &str, bytes: &[u8]) -> Result<()> {
    if path == "-" {
        std::io::stdout()
            .write_all(bytes)
            .map_err(|error| error.to_string())
    } else {
        std::fs::write(path, bytes).map_err(|error| format!("{path}: {error}"))
    }
}

fn run(command: Command) -> Result<()> {
    let input = read_input(&command.table)?;
    let output = tpde_fadec_gen::generate(&input, &command.options)?;
    write_output(&command.public, output.public_rs.as_bytes())?;
    write_output(&command.private, output.private_rs.as_bytes())?;
    if let Some(path) = command.forms {
        write_output(&path, output.forms_tsv.as_bytes())?;
    }
    if let Some(path) = command.semantic {
        write_output(&path, output.semantic_tsv.as_bytes())?;
    }
    if command.stats && !output.stats.is_empty() {
        println!("{}", output.stats);
    }
    Ok(())
}

fn main() {
    let command = match parse_args(std::env::args().skip(1)) {
        Ok(Some(command)) => command,
        Ok(None) => {
            println!("{USAGE}");
            return;
        }
        Err(error) => {
            eprintln!("fadec-gen: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(command) {
        eprintln!("fadec-gen: {error}");
        std::process::exit(1);
    }
}
