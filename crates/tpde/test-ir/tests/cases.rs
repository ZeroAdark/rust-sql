// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::string::String;

pub struct Case {
    pub name: &'static str,
    pub source: &'static str,
    pub accepted: bool,
    pub analyze: bool,
}
macro_rules! good {
    ($name:literal, $text:literal) => {
        Case {
            name: $name,
            source: $text,
            accepted: true,
            analyze: true,
        }
    };
}
macro_rules! parse_only {
    ($name:literal, $text:literal) => {
        Case {
            name: $name,
            source: $text,
            accepted: true,
            analyze: false,
        }
    };
}
macro_rules! bad {
    ($name:literal, $text:literal) => {
        Case {
            name: $name,
            source: $text,
            accepted: false,
            analyze: false,
        }
    };
}
pub const CASES: &[Case] = &[
    good!("vertical_tab", "f(\x0b%a) {\nentry:\nret %a\n}\n"),
    good!("empty", ""),
    good!("comments", "; first\n  ; second\n"),
    good!("terminate", "f() {\nentry:\nterminate\n}\n"),
    good!("trap", "f() {\nentry:\ntrap\n}\n"),
    good!("return_argument", "f(%a!) {\nentry:\nret %a\n}\n"),
    good!(
        "arithmetic_forced",
        "f(%a,%b) {\nentry:\n%x! = add %a, %b\n%y = sub %x, %a\nret %y\n}\n"
    ),
    good!(
        "legacy_any",
        "f(%a) {\nentry:\n%x =\n%y = %x, %a\nret %y\n}\n"
    ),
    good!(
        "any_many",
        "f(%a,%b,%c) {\nentry:\n%x = any %a, %b, %c, %a\nret %x\n}\n"
    ),
    good!(
        "allocation_and_immediates",
        "f(%a,%b) {\nentry:\n%p = alloca 0xffffffff, 0x10\n%z = zerofill 4294967295\n%x = condselect %a, %b, %a, %z, 9\nret %x\n}\n"
    ),
    good!("branch", "f(%a) {\nentry:\nbr ^exit\nexit:\nret %a\n}\n"),
    good!(
        "diamond_phi",
        "f(%a,%b) {\nentry:\ncondbr %a, ^left, ^right\nleft:\n%x = add %a, %b\nbr ^merge\nright:\n%y = sub %a, %b\nbr ^merge\nmerge:\n%z! = phi [^left, %x], [^right, %y]\nret %z\n}\n"
    ),
    good!(
        "tbz",
        "f(%a) {\nentry:\ntbz %a, ^left, ^right, 63\nleft:\nret %a\nright:\ntrap\n}\n"
    ),
    good!(
        "jump",
        "f() {\nentry:\njump ^first, ^second, ^third\nfirst:\nterminate\nsecond:\ntrap\nthird:\nterminate\n}\n"
    ),
    good!("jump_zero", "f() {\nentry:\njump\n}\n"),
    good!(
        "declarations_forward_call",
        "define @main(%a) {\nentry:\n%x = call @external, %a\n%y = call @localfn, %x\nret %y\n}\ndeclare @external(%x)\nlocalfn(%b) local {\nbody:\nret %b\n}\nlegacy(%x)!\n"
    ),
    good!(
        "zero_arg_call",
        "f() {\nentry:\n%x = call @g\nret %x\n}\ng()!\n"
    ),
    good!(
        "self_loop",
        "f(%a) {\nentry:\nbr ^loop\nloop:\ncondbr %a, ^loop, ^exit\nexit:\nret %a\n}\n"
    ),
    good!(
        "loop_phi_forward",
        "f(%n,%one) {\nentry:\nbr ^loop\nloop:\n%i = phi [^entry, %n], [^loop, %next]\n%next = sub %i, %one\ncondbr %next, ^loop, ^exit\nexit:\nret %next\n}\n"
    ),
    good!(
        "nested_loops",
        "f(%a) {\nentry:\nbr ^outer\nouter:\nbr ^inner\ninner:\ncondbr %a, ^inner, ^latch\nlatch:\ncondbr %a, ^outer, ^exit\nexit:\nret %a\n}\n"
    ),
    good!(
        "irreducible",
        "f(%a) {\nentry:\ncondbr %a, ^left, ^right\nleft:\nbr ^right\nright:\ncondbr %a, ^left, ^exit\nexit:\nret %a\n}\n"
    ),
    good!(
        "unreachable",
        "f(%a) {\nentry:\nret %a\nunreachable:\n%z = any\nret %z\n}\n"
    ),
    good!(
        "duplicate_successor",
        "f(%a) {\nentry:\ncondbr %a, ^exit, ^exit\nexit:\nret %a\n}\n"
    ),
    good!(
        "nonentry_alloca",
        "f() {\nentry:\nbr ^body\nbody:\n%p = alloca 0, 0\nret %p\n}\n"
    ),
    good!(
        "inline_comment_continuation",
        "f(%a,\n%b) {\nentry:\n%x = add %a, ; continuation\n%b\nret %x ; end\n}\n"
    ),
    good!(
        "uppercase_hex",
        "f() {\nentry:\n%x = zerofill 0xABCDEF\nret %x\n}\n"
    ),
    good!("whitespace", "f(\t%a\r\n)\t{\r\nentry:\r\nret\t%a\r\n}\r\n"),
    parse_only!("empty_function", "f() {}\n"),
    parse_only!(
        "empty_names_compatibility",
        "define @() {\n:\nterminate\n}\n"
    ),
    parse_only!(
        "duplicate_value_last_definition",
        "f(%a,%a) {\nentry:\n%x = any\n%x = any\nret %x\n}\n"
    ),
    parse_only!(
        "forward_value_no_dominance",
        "f() {\nentry:\n%x = add %y, %y\n%y = any\nret %x\n}\n"
    ),
    parse_only!("zero_phi", "f() {\nentry:\n%x = phi\nterminate\n}\n"),
    parse_only!(
        "duplicate_phi_predecessor",
        "f(%a,%b) {\nentry:\nbr ^exit\nexit:\n%x = phi [^entry, %a], [^entry, %b]\nret %x\n}\n"
    ),
    parse_only!(
        "named_terminator_unnamed_definition",
        "f() {\nentry:\nany\n%x = terminate\n}\n"
    ),
    bad!("invalid_function_name", "1f() {}\n"),
    bad!("missing_llvm_at", "define f() {}\n"),
    bad!("duplicate_function", "f()!\nf()!\n"),
    bad!("missing_lparen", "f {}\n"),
    bad!("missing_arg_percent", "f(a)!\n"),
    bad!("invalid_arg_name", "f(%1)!\n"),
    bad!("missing_arg_comma", "f(%a %b)!\n"),
    bad!("trailing_arg_comma", "f(%a,)!\n"),
    bad!("missing_lbrace", "f() entry:\nterminate\n"),
    bad!("missing_block_colon", "f() {\nentry\nterminate\n}\n"),
    bad!(
        "duplicate_block",
        "f() {\nentry:\nterminate\nentry:\nterminate\n}\n"
    ),
    bad!(
        "missing_assignment_equals",
        "f() {\nentry:\n%x any\nterminate\n}\n"
    ),
    bad!(
        "invalid_definition_name",
        "f() {\nentry:\n%1 = any\nterminate\n}\n"
    ),
    bad!(
        "unknown_operation",
        "f() {\nentry:\n%x = unknown\nterminate\n}\n"
    ),
    bad!(
        "operand_count",
        "f(%a) {\nentry:\n%x = add %a\nterminate\n}\n"
    ),
    bad!("successor_count", "f() {\nentry:\nbr\n}\n"),
    bad!(
        "immediate_count",
        "f() {\nentry:\n%x = alloca 4\nterminate\n}\n"
    ),
    bad!("undefined_value", "f() {\nentry:\nret %missing\n}\n"),
    bad!("undefined_block", "f() {\nentry:\nbr ^missing\n}\n"),
    bad!(
        "call_missing_at",
        "f() {\nentry:\n%x = call foo\nterminate\n}\n"
    ),
    bad!(
        "undefined_function",
        "f() {\nentry:\n%x = call @missing\nterminate\n}\n"
    ),
    bad!(
        "call_argument_count",
        "f() {\nentry:\n%x = call @g\nterminate\n}\ng(%a)!\n"
    ),
    bad!(
        "negative_immediate",
        "f() {\nentry:\n%x = zerofill -1\nterminate\n}\n"
    ),
    bad!(
        "plus_immediate",
        "f() {\nentry:\n%x = zerofill +1\nterminate\n}\n"
    ),
    bad!(
        "overflow_immediate",
        "f() {\nentry:\n%x = zerofill 4294967296\nterminate\n}\n"
    ),
    bad!(
        "hex_overflow",
        "f() {\nentry:\n%x = zerofill 0x100000000\nterminate\n}\n"
    ),
    bad!(
        "hex_no_digits",
        "f() {\nentry:\n%x = zerofill 0x\nterminate\n}\n"
    ),
    bad!(
        "value_after_block",
        "f(%a) {\nentry:\ncondbr ^entry, %a, ^entry\n}\n"
    ),
    bad!(
        "value_after_immediate",
        "f(%a) {\nentry:\n%x = condselect 1, %a\nterminate\n}\n"
    ),
    bad!("block_after_immediate", "f() {\nentry:\nbr 1, ^entry\n}\n"),
    bad!(
        "phi_missing_caret",
        "f(%a) {\nentry:\n%x = phi [%entry, %a]\nterminate\n}\n"
    ),
    bad!(
        "phi_missing_comma",
        "f(%a) {\nentry:\n%x = phi [^entry %a]\nterminate\n}\n"
    ),
    bad!(
        "phi_missing_percent",
        "f(%a) {\nentry:\n%x = phi [^entry, a]\nterminate\n}\n"
    ),
    bad!(
        "phi_missing_bracket",
        "f(%a) {\nentry:\n%x = phi [^entry, %a\nterminate\n}\n"
    ),
    bad!(
        "phi_not_first",
        "f(%a) {\nentry:\n%x = any\n%y = phi [^entry, %a]\nterminate\n}\n"
    ),
    bad!("missing_terminator", "f() {\nentry:\n%x = any\n}\n"),
    bad!("missing_rbrace", "f() {\nentry:\nterminate\n"),
    bad!(
        "immediate_garbage_suffix",
        "f() {\nentry:\n%x = zerofill 1z\nterminate\n}\n"
    ),
    bad!("invalid_operand_name", "f() {\nentry:\nret %1\n}\n"),
    bad!("invalid_successor_name", "f() {\nentry:\nbr ^1\n}\n"),
    bad!(
        "invalid_call_name",
        "f() {\nentry:\n%x = call @1\nterminate\n}\n"
    ),
    bad!(
        "invalid_phi_block_name",
        "f() {\nentry:\n%x = phi [^1, %x]\nterminate\n}\n"
    ),
    bad!(
        "invalid_phi_value_name",
        "f() {\nentry:\n%x = phi [^entry, %1]\nterminate\n}\n"
    ),
];

pub fn linear(blocks: u32) -> String {
    use std::fmt::Write;
    let mut text = String::from("scaled(%a) {\n");
    for i in 0..blocks {
        writeln!(text, "b{}:\n%x{} = any %a", i, i).unwrap();
        if i + 1 < blocks {
            writeln!(text, "br ^b{}", i + 1).unwrap();
        } else {
            writeln!(text, "ret %x{}", i).unwrap();
        }
    }
    text.push_str("}\n");
    text
}
