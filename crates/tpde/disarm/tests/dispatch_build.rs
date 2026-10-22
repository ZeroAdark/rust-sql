// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;

pub fn generate(public: &str, private: &str) -> String {
    let mut functions = BTreeMap::new();
    for source in [public, private] {
        for fragment in source.split("pub fn de64_").skip(1) {
            let (name, rest) = fragment.split_once('(').expect("encoder name");
            let (params, _) = rest.split_once(')').expect("encoder parameters");
            assert!(functions.insert(name, params).is_none(), "duplicate {name}");
        }
    }
    assert!(
        functions.len() >= 3000,
        "missing generated encoder functions"
    );
    let mut names = String::from("pub const ENCODER_NAMES: &[&str] = &[\n");
    let mut dispatch = String::from("pub fn encode(id: usize, args: &[u64]) -> u32 { match id {\n");
    for (id, (name, params)) in functions.into_iter().enumerate() {
        names.push_str(&format!("{name:?},\n"));
        let mut arguments = Vec::new();
        for (index, param) in params
            .split(',')
            .filter(|p| !p.trim().is_empty())
            .enumerate()
        {
            let (_, ty) = param.split_once(':').expect("typed encoder parameter");
            let value = format!("args[{index}]");
            arguments.push(match ty.trim() {
                ty @ ("GReg" | "GRegZr" | "GRegSp" | "VReg") => {
                    format!("tpde_disarm::runtime::{ty}::wrapping({value} as u32)")
                }
                "Cond" | "PrfOp" => format!("({value} as u32)"),
                "bool" => format!("({value} != 0)"),
                "f32" => format!("f32::from_bits({value} as u32)"),
                "f64" => format!("f64::from_bits({value})"),
                ty @ ("u8" | "u32" | "u64" | "i32" | "i64" | "usize" | "isize") => {
                    format!("({value} as {ty})")
                }
                ty => panic!("unknown encoder parameter type {ty}"),
            });
        }
        dispatch.push_str(&format!(
            "{id} => tpde_disarm::de64_{name}({}),\n",
            arguments.join(", ")
        ));
    }
    names.push_str("];\n");
    dispatch.push_str("_ => panic!(\"unknown encoder {id}\"), } }\n");
    names + &dispatch
}
