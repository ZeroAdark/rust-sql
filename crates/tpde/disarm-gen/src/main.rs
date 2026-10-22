// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::{env, fs, process::ExitCode};
use tpde_disarm_gen::{generate, Options};

fn negative_number(value: &str) -> bool {
    let Some(number) = value.strip_prefix('-') else {
        return false;
    };
    let number = number.strip_suffix('\n').unwrap_or(number);
    let digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
    if let Some((integer, fraction)) = number.split_once('.') {
        digits(integer) && !fraction.is_empty() && digits(fraction)
    } else {
        !number.is_empty() && digits(number)
    }
}

fn option_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    let error = || format!("{option} requires a value");
    let value = args.next().ok_or_else(error)?;
    if value.starts_with('-') && value != "-" {
        let name = value
            .split_once('=')
            .map_or(value.as_str(), |(name, _)| name);
        let known = matches!(
            name,
            "--features"
                | "--feature-desc"
                | "--manifest"
                | "--coverage"
                | "--encode-in-header"
                | "--reference-c"
                | "--help"
                | "-h"
        );
        if value == "--"
            || known
            || value.starts_with("-h")
            || (!negative_number(&value) && !value.contains(' '))
        {
            return Err(error());
        }
    }
    Ok(value)
}

fn run() -> Result<(), String> {
    let mut options = Options::default();
    let mut feature_file = None;
    let mut manifest = None;
    let mut reference_c = false;
    let mut paths = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--features" => options.features = Some(value.to_owned()),
                "--feature-desc" => feature_file = Some(value.to_owned()),
                "--manifest" => manifest = Some(value.to_owned()),
                _ if option.starts_with('-') => return Err(format!("unknown option {option}")),
                _ => {
                    paths.push(arg);
                    continue;
                }
            }
            continue;
        }
        match arg.as_str() {
            "--features" => options.features = Some(option_value(&mut args, "--features")?),
            "--coverage" => options.coverage = true,
            "--encode-in-header" => options.encode_in_header = true,
            "--feature-desc" => feature_file = Some(option_value(&mut args, "--feature-desc")?),
            "--manifest" => manifest = Some(option_value(&mut args, "--manifest")?),
            "--reference-c" => reference_c = true,
            "--help" | "-h" => {
                println!(
                    "disarm-gen [--features FEATURES] [--coverage] [--encode-in-header] [--feature-desc PATH] [--manifest PATH] [--reference-c] PUBLIC PRIVATE DESC..."
                );
                return Ok(());
            }
            "--" => {
                paths.extend(args);
                break;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => paths.push(arg),
        }
    }
    options.features = options.features.filter(|value| value != "--");
    feature_file = feature_file.filter(|value| !value.is_empty() && value != "--");
    if paths.len() < 3 {
        return Err("expected PUBLIC PRIVATE and at least one DESC input".into());
    }
    let sources = paths[2..]
        .iter()
        .map(|p| fs::read_to_string(p).map_err(|e| format!("{p}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let feature = feature_file
        .map(|p| fs::read_to_string(&p).map_err(|e| format!("{p}: {e}")))
        .transpose()?;
    let generated = generate(
        &sources.iter().map(String::as_str).collect::<Vec<_>>(),
        feature.as_deref(),
        &options,
    )?;
    let (public, private) = if reference_c {
        (&generated.reference_public, &generated.reference_private)
    } else {
        (&generated.public_rust, &generated.private_rust)
    };
    fs::write(&paths[0], public).map_err(|e| format!("{}: {e}", paths[0]))?;
    fs::write(&paths[1], private).map_err(|e| format!("{}: {e}", paths[1]))?;
    if let Some(path) = manifest {
        fs::write(&path, &generated.manifest).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("disarm-gen: {error}");
            ExitCode::FAILURE
        }
    }
}
