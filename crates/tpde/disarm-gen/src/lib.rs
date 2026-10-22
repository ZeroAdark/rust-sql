// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

pub mod decoder;
pub mod encoder;
pub mod expression;
pub mod parser;

use parser::{parse_desc, Desc};

pub const UPSTREAM_REVISION: &str = "2d13d3f410a52daff1c5d8ef07d623332f372560";
pub const PROTOCOL_VERSION: u32 = 1;
const MAX_INPUT_BYTES: usize = 16 << 20;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub features: Option<String>,
    pub coverage: bool,
    pub encode_in_header: bool,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub name: String,
    pub props: Vec<(String, String)>,
    pub desc: Desc,
}

impl Group {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Generated {
    pub public_rust: String,
    pub private_rust: String,
    pub manifest: String,
    pub reference_public: String,
    pub reference_private: String,
}

pub fn check_feature(features: &[(String, bool)], expression: &str) -> Result<bool, String> {
    for name in expression.split("&&") {
        let enabled = features
            .iter()
            .find(|(k, _)| k == name)
            .ok_or_else(|| format!("unknown feature {name}"))?
            .1;
        if !enabled {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone, Debug, Default)]
pub struct Section {
    pub name: String,
    pub props: Vec<(String, String)>,
}

fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn ini_lines(mut input: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if input.is_empty() {
            return None;
        }
        let end = input.find(['\r', '\n']).unwrap_or(input.len());
        let line = &input[..end];
        input = &input[end..];
        if let Some(rest) = input.strip_prefix('\r') {
            input = rest;
        }
        if let Some(rest) = input.strip_prefix('\n') {
            input = rest;
        }
        Some(line)
    })
}

pub fn parse_ini(inputs: &[&str]) -> Result<Vec<Section>, String> {
    if inputs
        .iter()
        .try_fold(0usize, |n, s| n.checked_add(s.len()))
        .is_none_or(|n| n > MAX_INPUT_BYTES)
    {
        return Err("input byte limit exceeded".into());
    }
    let mut sections: Vec<Section> = vec![];
    let mut defaults: Vec<(String, String)> = vec![];
    for input in inputs {
        let mut current: Option<usize> = None;
        let mut in_default = false;
        let mut last: Option<(usize, usize)> = None;
        let mut seen_sections: Vec<String> = vec![];
        let mut seen_keys: Vec<(String, String)> = vec![];
        for (lineno, raw) in ini_lines(input).enumerate() {
            let line = raw.trim_matches(python_whitespace);
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let indent = raw.chars().take_while(|&c| python_whitespace(c)).count();
            if line.is_empty() {
                if let Some((_, key)) = last {
                    let props = if in_default {
                        &mut defaults
                    } else {
                        &mut sections[current.unwrap()].props
                    };
                    props[key].1.push('\n');
                }
                continue;
            }
            if let Some((prior_indent, key)) = last {
                if indent > prior_indent {
                    let props = if in_default {
                        &mut defaults
                    } else {
                        &mut sections[current.unwrap()].props
                    };
                    props[key].1.push('\n');
                    props[key].1.push_str(line);
                    continue;
                }
            }
            if let Some(close) = line
                .starts_with('[')
                .then(|| line.rfind(']'))
                .flatten()
                .filter(|close| *close > 1)
            {
                let name = &line[1..close];
                if name != "DEFAULT" && seen_sections.iter().any(|s| s == name) {
                    return Err(format!("duplicate section {name}"));
                }
                seen_sections.push(name.into());
                in_default = name == "DEFAULT";
                if !in_default {
                    current = Some(
                        if let Some(pos) = sections.iter().position(|s| s.name == name) {
                            pos
                        } else {
                            sections.push(Section {
                                name: name.into(),
                                props: vec![],
                            });
                            sections.len() - 1
                        },
                    );
                }
                last = None;
                continue;
            }
            if !in_default && current.is_none() {
                return Err(format!("property before section at line {}", lineno + 1));
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("missing property delimiter at line {}", lineno + 1))?;
            let key = key.trim_end_matches(python_whitespace);
            if key.is_empty() {
                return Err("empty property key".into());
            }
            let section_name = if in_default {
                "DEFAULT"
            } else {
                &sections[current.unwrap()].name
            };
            if seen_keys.iter().any(|(s, k)| s == section_name && k == key) {
                return Err(format!("duplicate property {section_name}:{key}"));
            }
            seen_keys.push((section_name.into(), key.into()));
            let props = if in_default {
                &mut defaults
            } else {
                &mut sections[current.unwrap()].props
            };
            let pos = if let Some(pos) = props.iter().position(|(k, _)| k == key) {
                props[pos].1 = value.trim_matches(python_whitespace).into();
                pos
            } else {
                props.push((key.into(), value.trim_matches(python_whitespace).into()));
                props.len() - 1
            };
            last = Some((indent, pos));
        }
    }
    for section in &mut sections {
        for (key, value) in &defaults {
            if !section.props.iter().any(|(k, _)| k == key) {
                section.props.push((key.clone(), value.clone()));
            }
        }
        for (_, value) in &mut section.props {
            *value = value.trim_end_matches(python_whitespace).into();
        }
    }
    Ok(sections)
}

pub fn select_features(
    description: Option<&str>,
    selected: Option<&str>,
) -> Result<Vec<(String, bool)>, String> {
    let mut features = vec![(String::new(), true)];
    let Some(description) = description else {
        return Ok(features);
    };
    let mut attrs: Vec<(String, Vec<String>)> = vec![];
    fn add(attrs: &mut Vec<(String, Vec<String>)>, key: &str, value: &str) {
        let pos = if let Some(pos) = attrs.iter().position(|(k, _)| k == key) {
            pos
        } else {
            attrs.push((key.into(), vec![]));
            attrs.len() - 1
        };
        if !attrs[pos].1.iter().any(|v| v == value) {
            attrs[pos].1.push(value.into());
        }
    }
    fn merge(attrs: &mut Vec<(String, Vec<String>)>, into: String, from: String) {
        let values = attrs
            .iter()
            .find(|(k, _)| k == &from)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        for value in values {
            add(attrs, &into, &value);
        }
    }
    for section in parse_ini(&[description])? {
        features.push((section.name.clone(), false));
        add(&mut attrs, &section.name.to_lowercase(), &section.name);
        if let Some((_, avail)) = section.props.iter().find(|(k, _)| k == "avail") {
            add(&mut attrs, avail, &section.name);
        }
        if !section.props.iter().any(|(k, _)| k == "incomplete") {
            add(&mut attrs, "all", &section.name);
        }
    }
    for i in 1..10 {
        merge(
            &mut attrs,
            format!("armv8.{i}a"),
            format!("armv8.{}a", i - 1),
        );
    }
    for i in 0..5 {
        merge(
            &mut attrs,
            format!("armv9.{i}a"),
            format!("armv8.{}a", i + 5),
        );
    }
    for i in 1..5 {
        merge(
            &mut attrs,
            format!("armv9.{i}a"),
            format!("armv9.{}a", i - 1),
        );
    }
    for (name, _) in &features {
        add(&mut attrs, "really-all", name);
    }
    for attr in selected
        .filter(|value| !value.is_empty())
        .unwrap_or("all")
        .trim_matches(python_whitespace)
        .split(',')
    {
        let values = &attrs
            .iter()
            .find(|(k, _)| k == attr)
            .filter(|(_, v)| !v.is_empty())
            .ok_or_else(|| format!("unknown feature {attr}"))?
            .1;
        for name in values {
            features.iter_mut().find(|(k, _)| k == name).unwrap().1 = true;
        }
    }
    Ok(features)
}

/// Generated sources own their text after the borrowed inputs are released.
///
/// ```
/// use tpde_disarm_gen::{generate, Options};
/// let result = {
///     let input = String::from("[BASE]\ndesc: 00000000000000000000000000000000\nmnem: BASE\ndecode: base\nencmnem: BASE\nencops:\n");
///     let options = Options::default();
///     generate(&[&input], None, &options).unwrap()
/// };
/// assert!(result.public_rust.contains("DA64I_BASE"));
/// assert!(result.manifest.contains("emitted\tBASE\tBASE\t0\n"));
/// ```
pub fn generate(
    descfiles: &[&str],
    feature_desc: Option<&str>,
    options: &Options,
) -> Result<Generated, String> {
    let features = select_features(feature_desc, options.features.as_deref())?;
    let mut decoder = decoder::Decoder::new();
    let mut encoder = encoder::Encoder::new(options.encode_in_header);
    for section in parse_ini(descfiles)? {
        let source = section
            .props
            .iter()
            .find(|(k, _)| k == "desc")
            .ok_or_else(|| format!("group {} without desc", section.name))?;
        let desc = parse_desc(&source.1).map_err(|e| format!("{}: {e}", section.name))?;
        let group = Group {
            name: section.name,
            props: section.props,
            desc,
        };
        decoder.add_group(&group, &features)?;
        encoder.add_group(&group, &features)?;
    }
    let decode = decoder.generate(options.coverage)?;
    let encode = encoder.generate()?;
    let mut result = Generated::default();
    result.manifest = format!(
        "protocol\t{PROTOCOL_VERSION}\nupstream\t{UPSTREAM_REVISION}\ncoverage\t{}\ninline\t{}\n",
        options.coverage as u8, options.encode_in_header as u8
    );
    for (name, enabled) in &features {
        result
            .reference_public
            .push_str(&format!("#define DA64_HAVE_{name} {}\n", *enabled as u8));
        result
            .public_rust
            .push_str(&format!("pub const DA64_HAVE_{name}: bool = {enabled};\n"));
        result
            .manifest
            .push_str(&format!("feature\t{name}\t{}\n", *enabled as u8));
    }
    result.public_rust.push_str(&decode.public);
    result.public_rust.push_str(&encode.public);
    result.private_rust = decode.private + &encode.private;
    result.manifest.push_str(&decode.manifest);
    result.manifest.push_str(&encode.manifest);
    result.reference_public.push_str(&decode.public_c);
    result.reference_public.push_str(&encode.public_c);
    for (i, (key, value)) in decode
        .private_c
        .into_iter()
        .chain(encode.private_c)
        .enumerate()
    {
        result.reference_private.push_str(&format!(
            "#{}if defined({key})\n{value}\n",
            if i == 0 { "" } else { "el" }
        ));
    }
    result
        .reference_private
        .push_str("#else\n#error \"unknown table\"\n#endif\n");
    Ok(result)
}
