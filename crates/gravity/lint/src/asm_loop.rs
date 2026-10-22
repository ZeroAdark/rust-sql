//! The hot-loop lint over the assembly corpus (docs/gravity/expr.md §2.3, ruling 3): the served
//! kernels' loops cut from a post-LTO `--emit asm`, annotated by operation class, checked for
//! calls other than cold stubs and for stack traffic of loop-carried values.
use std::fs;
use std::path::PathBuf;

/// The served kernels the corpus carries: (corpus file stem, the entry's demangled path).
pub const KERNELS: &[(&str, &str)] = &[
    ("par_scan_count", "gravity_kernels::par_scan_count::par_scan_count"),
    ("par_hashagg", "gravity_kernels::par_hashagg::par_hashagg"),
    ("par_hashjoin_probe", "gravity_kernels::par_hashjoin::par_hashjoin_probe"),
];

/// Out-of-line targets a hot loop may branch to: the arena refill and the panic paths.
pub const COLD_STUBS: &[&str] = &["arena_grow", "7HashAgg4grow", "7HashAgg8open_new", "9HashBuild4grow", "panic", "unwrap_failed", "expect_failed", "slice_index", "slice_start_index", "slice_end_index", "len_mismatch", "assert_failed", "__rust_probestack", "handle_alloc_error", "unreachable", "copy_from_slice"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Load,
    Gather,
    Store,
    StackLoad,
    StackStore,
    StackIndexed,
    Test,
    Branch,
    Or,
    Alu,
    Call,
    IndirectCall,
    ColdCall,
    Label,
}

impl Class {
    pub fn tag(self) -> &'static str {
        match self {
            Class::Load => "load",
            Class::Gather => "gather",
            Class::Store => "store",
            Class::StackLoad => "STACK load",
            Class::StackStore => "STACK store",
            Class::StackIndexed => "stack array",
            Class::Test => "test",
            Class::Branch => "branch",
            Class::Or => "flag or",
            Class::Alu => "alu",
            Class::Call => "CALL",
            Class::IndirectCall => "CALL indirect",
            Class::ColdCall => "cold stub",
            Class::Label => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Insn {
    pub text: String,
    pub class: Class,
    pub target: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Func {
    pub symbol: String,
    pub demangled: String,
    pub items: Vec<Insn>,
}

/// A natural loop: the header block and the blocks that reach its back edges without leaving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loop {
    pub header: usize,
    pub blocks: Vec<usize>,
    pub label: String,
    pub innermost: bool,
    pub window: bool,
    pub hot: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kernel: String,
    pub label: String,
    pub what: String,
}

impl Finding {
    pub fn line(&self) -> String {
        format!("{} {} {}", self.kernel, self.label, self.what)
    }
}

pub fn corpus_dir() -> PathBuf {
    crate::gravity_root().parent().unwrap().parent().unwrap().join("crates/gravity/lint/testdata/gravity/asm")
}

/// Legacy Rust mangling (`_ZN<len><ident>…17h<hash>E`, a Mach-O leading `_` allowed) to a path.
pub fn demangle_legacy(sym: &str) -> Option<String> {
    let body = sym.strip_prefix("__ZN").or_else(|| sym.strip_prefix("_ZN"))?.strip_suffix('E')?;
    let mut rest = body;
    let mut segs: Vec<String> = Vec::new();
    while !rest.is_empty() {
        let n: usize = rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        let (seg, tail) = rest[digits..].split_at_checked(n)?;
        rest = tail;
        segs.push(seg.to_string());
    }
    if segs.last().is_some_and(|s| s.len() == 17 && s.starts_with('h') && s[1..].chars().all(|c| c.is_ascii_hexdigit())) {
        segs.pop();
    }
    let joined = segs.join("::");
    let pairs = [("$LT$", "<"), ("$GT$", ">"), ("$RF$", "&"), ("$LP$", "("), ("$RP$", ")"), ("$C$", ","), ("$u20$", " "), ("$u7b$", "{"), ("$u7d$", "}"), ("$BP$", "*"), ("$u5b$", "["), ("$u5d$", "]"), ("..", "::")];
    Some(pairs.iter().fold(joined, |s, (a, b)| s.replace(a, b)))
}

fn is_local_label(line: &str) -> bool {
    let l = line.strip_prefix('.').unwrap_or(line);
    l.starts_with('L') && l.ends_with(':') && !l.contains(char::is_whitespace)
}

fn is_function_label(line: &str) -> bool {
    line.ends_with(':') && !line.starts_with(['.', '\t', ' ']) && !is_local_label(line) && line.len() > 1
}

/// `;` (Mach-O), `//` (ELF AArch64) and `# ` (x86 AT&T) open comments; `#imm` is an operand.
fn strip_comment(line: &str) -> &str {
    let mut end = line.len();
    for (i, c) in line.char_indices() {
        let hash_comment = c == '#' && line[i + 1..].chars().next().is_none_or(|n| n == ' ');
        if c == ';' || hash_comment || line[i..].starts_with("//") {
            end = i;
            break;
        }
    }
    line[..end].trim_end()
}

fn mem_operand(ops: &str) -> Option<&str> {
    if let Some(a) = ops.find('[') {
        let b = ops[a..].find(']')? + a;
        return Some(&ops[a..=b]);
    }
    let a = ops.find('(')?;
    let b = ops[a..].find(')')? + a;
    Some(&ops[a..=b])
}

fn stack_mem(m: &str) -> bool {
    m.starts_with("[sp") || m.starts_with("[x29") || m.contains("(%rsp") || m.contains("(%rbp") || m.contains("(%esp") || m.contains("(%ebp")
}

fn indexed_mem(m: &str) -> bool {
    if m.starts_with('[') {
        let inner = &m[1..m.len() - 1];
        let mut parts = inner.split(',').map(str::trim);
        parts.next();
        return parts.next().is_some_and(|p| p.starts_with(['x', 'w']) && p[1..].chars().next().is_some_and(|c| c.is_ascii_digit() || c == 'z'));
    }
    let inner = &m[m.find('(').unwrap_or(0)..];
    inner.matches(',').count() >= 1
}

pub fn classify(mn: &str, ops: &str) -> (Class, Option<String>) {
    let last = ops.rsplit(',').next().map(str::trim).unwrap_or("").to_string();
    let first = ops.split(',').next().map(str::trim).unwrap_or("").to_string();
    match mn {
        "bl" => return (if COLD_STUBS.iter().any(|c| ops.contains(c)) { Class::ColdCall } else { Class::Call }, Some(ops.trim().to_string())),
        "blr" => return (Class::IndirectCall, Some(ops.trim().to_string())),
        "call" | "callq" => {
            let t = ops.trim().to_string();
            return (if t.starts_with('*') { Class::IndirectCall } else if COLD_STUBS.iter().any(|c| t.contains(c)) { Class::ColdCall } else { Class::Call }, Some(t));
        }
        "b" | "br" | "ret" | "cbz" | "cbnz" | "tbz" | "tbnz" | "jmp" | "retq" => return (Class::Branch, (!ops.is_empty()).then_some(if mn.starts_with('j') { first } else { last })),
        _ => {}
    }
    if mn.starts_with("b.") || (mn.starts_with('j') && mn.len() <= 4) {
        return (Class::Branch, Some(if mn.starts_with('j') { first } else { last }));
    }
    if let Some(m) = mem_operand(ops) {
        let store = mn.starts_with("st") || (m.starts_with('(') && ops.trim_end().ends_with(')') && !matches!(mn.trim_end_matches(['b', 'w', 'l', 'q']), "cmp" | "test" | "bt"));
        if mn == "lea" || mn == "leaq" || mn == "leal" || mn == "adr" || mn == "adrp" {
            return (Class::Alu, None);
        }
        if stack_mem(m) {
            return (if indexed_mem(m) { Class::StackIndexed } else if store { Class::StackStore } else { Class::StackLoad }, None);
        }
        if matches!(mn.trim_end_matches(['b', 'w', 'l', 'q']), "cmp" | "test" | "tst" | "bt") {
            return (Class::Test, None);
        }
        return (if store { Class::Store } else if indexed_mem(m) { Class::Gather } else { Class::Load }, None);
    }
    if matches!(mn, "tst" | "cmp" | "cmn" | "ccmp" | "ccmn" | "fcmp" | "test" | "testb" | "testl" | "testq" | "bt" | "btl" | "btq") || mn.starts_with("cmp") {
        return (Class::Test, None);
    }
    if matches!(mn, "orr" | "orn" | "or" | "orl" | "orq" | "orb" | "orw") {
        return (Class::Or, None);
    }
    (Class::Alu, None)
}

/// The functions of an `--emit asm` file: mangled symbol, its demangling, instructions and labels.
pub fn parse_functions(asm: &str) -> Vec<Func> {
    let mut out: Vec<Func> = Vec::new();
    let mut cur: Option<Func> = None;
    for raw in asm.lines() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if is_function_label(line) {
            if let Some(f) = cur.take() {
                out.push(f);
            }
            let symbol = line.trim_end_matches(':').to_string();
            let demangled = demangle_legacy(&symbol).unwrap_or_else(|| symbol.clone());
            cur = Some(Func { symbol, demangled, items: Vec::new() });
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };
        let t = line.trim_start();
        if t == ".cfi_endproc" {
            out.push(cur.take().unwrap());
            continue;
        }
        if is_local_label(t) {
            f.items.push(Insn { text: t.to_string(), class: Class::Label, target: None });
            continue;
        }
        if t.starts_with('.') {
            continue;
        }
        let (mn, ops) = t.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((t, ""));
        let (class, target) = classify(mn, ops);
        f.items.push(Insn { text: format!("{mn}\t{ops}").trim_end().to_string(), class, target });
    }
    if let Some(f) = cur.take() {
        out.push(f);
    }
    out
}

fn label_name(s: &str) -> &str {
    s.trim_end_matches(':')
}

fn unconditional(x: &Insn) -> bool {
    let mn = x.text.split('\t').next().unwrap_or("");
    matches!(mn, "b" | "br" | "ret" | "retq" | "jmp")
}

/// Basic blocks over the items: leaders are the first item, every label and the item after a
/// branch; `succs` are branch targets and fall-throughs.
pub fn blocks(f: &Func) -> Vec<(usize, usize, Vec<usize>)> {
    let n = f.items.len();
    let mut leader = vec![false; n + 1];
    leader[0] = true;
    for (i, x) in f.items.iter().enumerate() {
        if x.class == Class::Label {
            leader[i] = true;
        }
        if x.class == Class::Branch && i + 1 <= n {
            leader[i + 1] = true;
        }
    }
    let starts: Vec<usize> = (0..n).filter(|&i| leader[i]).collect();
    let block_of_item = |i: usize| starts.partition_point(|&s| s <= i) - 1;
    let mut out = Vec::new();
    for (b, &s) in starts.iter().enumerate() {
        let e = starts.get(b + 1).copied().unwrap_or(n);
        let mut succs = Vec::new();
        let last = &f.items[e - 1];
        if last.class == Class::Branch {
            if let Some(t) = &last.target {
                if let Some(pos) = f.items.iter().position(|x| x.class == Class::Label && label_name(&x.text) == t.as_str()) {
                    succs.push(block_of_item(pos));
                }
            }
            if !unconditional(last) && e < n {
                succs.push(b + 1);
            }
        } else if e < n {
            succs.push(b + 1);
        }
        out.push((s, e, succs));
    }
    out
}

fn dominators(blocks: &[(usize, usize, Vec<usize>)]) -> Vec<Vec<u64>> {
    let n = blocks.len();
    let words = n.div_ceil(64).max(1);
    let full = vec![!0u64; words];
    let mut dom = vec![full.clone(); n];
    dom[0] = vec![0; words];
    dom[0][0] = 1;
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, (_, _, succs)) in blocks.iter().enumerate() {
        for &s in succs {
            preds[s].push(b);
        }
    }
    loop {
        let mut changed = false;
        for b in 1..n {
            let mut d = full.clone();
            if preds[b].is_empty() {
                d = vec![0; words];
            }
            for &p in &preds[b] {
                for w in 0..words {
                    d[w] &= dom[p][w];
                }
            }
            d[b / 64] |= 1 << (b % 64);
            if d != dom[b] {
                dom[b] = d;
                changed = true;
            }
        }
        if !changed {
            return dom;
        }
    }
}

/// Natural loops: a back edge `u → v` with `v` dominating `u`; the body is `v` plus everything that
/// reaches `u` without passing `v`. innermost = containing no other loop's header; window = an
/// innermost loop with an indirect call (the per-window protocol); hot = an innermost loop with a
/// memory read and no indirect call.
pub fn loops(f: &Func) -> Vec<Loop> {
    let bl = blocks(f);
    let dom = dominators(&bl);
    let dominates = |v: usize, u: usize| (dom[u][v / 64] >> (v % 64)) & 1 == 1;
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); bl.len()];
    for (b, (_, _, succs)) in bl.iter().enumerate() {
        for &s in succs {
            preds[s].push(b);
        }
    }
    let mut out: Vec<Loop> = Vec::new();
    for (u, (_, _, succs)) in bl.iter().enumerate() {
        for &v in succs {
            if !dominates(v, u) {
                continue;
            }
            let mut body = vec![false; bl.len()];
            body[v] = true;
            let mut stack = vec![u];
            while let Some(b) = stack.pop() {
                if body[b] {
                    continue;
                }
                body[b] = true;
                stack.extend(preds[b].iter().copied());
            }
            let label = f.items[bl[v].0..bl[v].1].iter().find(|x| x.class == Class::Label).map(|x| label_name(&x.text).to_string()).unwrap_or_else(|| format!("block{v}"));
            match out.iter_mut().find(|l| l.header == v) {
                Some(l) => {
                    for (b, in_body) in body.iter().enumerate() {
                        if *in_body && !l.blocks.contains(&b) {
                            l.blocks.push(b);
                        }
                    }
                    l.blocks.sort_unstable();
                }
                None => out.push(Loop { header: v, blocks: (0..bl.len()).filter(|&b| body[b]).collect(), label, innermost: true, window: false, hot: false }),
            }
        }
    }
    let headers: Vec<usize> = out.iter().map(|l| l.header).collect();
    for l in out.iter_mut() {
        l.innermost = !headers.iter().any(|&h| h != l.header && l.blocks.contains(&h));
        let body: Vec<&Insn> = l.blocks.iter().flat_map(|&b| f.items[bl[b].0..bl[b].1].iter()).collect();
        l.window = l.innermost && body.iter().any(|x| x.class == Class::IndirectCall);
        l.hot = l.innermost && !l.window && body.iter().any(|x| matches!(x.class, Class::Load | Class::Gather | Class::StackLoad | Class::StackIndexed));
    }
    out.sort_by_key(|l| l.header);
    out
}

fn body_of<'a>(f: &'a Func, bl: &[(usize, usize, Vec<usize>)], l: &Loop) -> Vec<&'a Insn> {
    l.blocks.iter().flat_map(|&b| f.items[bl[b].0..bl[b].1].iter()).collect()
}

/// The function whose loops are the kernel's: the entry itself, or — when the entry only tail-calls
/// into another `gravity_kernels` function (the shared `run` of the frame kernels) — that callee.
pub fn resolve<'a>(funcs: &'a [Func], entry: &'a Func) -> &'a Func {
    let mut f = entry;
    for _ in 0..3 {
        if !loops(f).is_empty() {
            return f;
        }
        let Some(t) = f.items.iter().filter(|x| matches!(x.class, Class::Call | Class::Branch)).filter_map(|x| x.target.as_deref()).find(|t| t.contains("gravity_kernels")) else { return f };
        let Some(g) = funcs.iter().find(|g| g.symbol == t) else { return f };
        f = g;
    }
    f
}

pub fn lint_loop(kernel: &str, label: &str, body: &[Insn]) -> Vec<Finding> {
    let mut out = Vec::new();
    for x in body {
        let what = match x.class {
            Class::Call => format!("call inside the hot loop: {}", x.text),
            Class::IndirectCall => format!("indirect call inside the hot loop: {}", x.text),
            Class::StackLoad | Class::StackStore => format!("stack traffic inside the hot loop: {}", x.text),
            _ => continue,
        };
        out.push(Finding { kernel: kernel.to_string(), label: label.to_string(), what });
    }
    out
}

fn count(body: &[&Insn], f: impl Fn(Class) -> bool) -> usize {
    body.iter().filter(|x| f(x.class)).count()
}

/// The annotated corpus text of one kernel: a header, one line per innermost loop, then every
/// innermost loop's blocks with the class per instruction (hot loops first).
pub fn cut(f: &Func, kernel: &str, entry: &str, meta: &str) -> String {
    let bl = blocks(f);
    let ls = loops(f);
    let insns = f.items.iter().filter(|x| x.class != Class::Label).count();
    let inner: Vec<&Loop> = ls.iter().filter(|l| l.innermost).collect();
    let mut s = String::new();
    s.push_str(&format!("# gravity-asm corpus: kernel {kernel}\n# entry: {entry}\n# symbol: {}\n# {meta}\n", f.demangled));
    s.push_str(&format!("# function: {insns} instructions, {} blocks, {} loops, {} innermost: {} hot, {} window, {} other\n", bl.len(), ls.len(), inner.len(), inner.iter().filter(|l| l.hot).count(), inner.iter().filter(|l| l.window).count(), inner.iter().filter(|l| !l.hot && !l.window).count()));
    let mut ordered: Vec<&Loop> = inner.clone();
    ordered.sort_by_key(|l| (!l.hot, l.header));
    let kind = |l: &Loop| if l.hot { "hot" } else if l.window { "window" } else { "other" };
    for l in &ordered {
        let body = body_of(f, &bl, l);
        s.push_str(&format!(
            "# {} {}: {} instructions in {} blocks, loads {} (gather {}), stores {}, tests {}, flag-or {}, calls {} (indirect {}, cold {}), stack {}\n",
            kind(l),
            l.label,
            count(&body, |c| c != Class::Label),
            l.blocks.len(),
            count(&body, |c| matches!(c, Class::Load | Class::Gather)),
            count(&body, |c| c == Class::Gather),
            count(&body, |c| c == Class::Store),
            count(&body, |c| c == Class::Test),
            count(&body, |c| c == Class::Or),
            count(&body, |c| matches!(c, Class::Call | Class::IndirectCall | Class::ColdCall)),
            count(&body, |c| c == Class::IndirectCall),
            count(&body, |c| c == Class::ColdCall),
            count(&body, |c| matches!(c, Class::StackLoad | Class::StackStore))
        ));
    }
    for l in &ordered {
        s.push_str(&format!("## {} {} (blocks {:?})\n", kind(l), l.label, l.blocks));
        for x in body_of(f, &bl, l) {
            if x.class == Class::Label {
                s.push_str(&format!("{}\n", x.text));
            } else {
                s.push_str(&format!("\t{:<40} ; {}\n", x.text, x.class.tag()));
            }
        }
    }
    s
}

/// A corpus file's hot-loop blocks re-parsed and linted (the class is recomputed from the
/// instruction text, never trusted from the annotation).
pub fn corpus_findings(kernel: &str, text: &str) -> (Vec<Finding>, Vec<(String, usize)>) {
    let mut findings = Vec::new();
    let mut hot: Vec<(String, usize)> = Vec::new();
    let mut in_hot: Option<String> = None;
    let mut body: Vec<Insn> = Vec::new();
    let flush = |label: &Option<String>, body: &mut Vec<Insn>, findings: &mut Vec<Finding>, hot: &mut Vec<(String, usize)>| {
        if let Some(l) = label {
            findings.extend(lint_loop(kernel, l, body));
            hot.push((l.clone(), body.iter().filter(|x| x.class != Class::Label).count()));
        }
        body.clear();
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(&in_hot, &mut body, &mut findings, &mut hot);
            in_hot = rest.strip_prefix("hot ").map(|r| r.split_whitespace().next().unwrap_or("").to_string());
            continue;
        }
        if line.starts_with('#') || in_hot.is_none() {
            continue;
        }
        if is_local_label(line.trim()) {
            body.push(Insn { text: line.trim().to_string(), class: Class::Label, target: None });
            continue;
        }
        let code = line.rsplit_once(" ; ").map(|(a, _)| a).unwrap_or(line).trim();
        let (mn, ops) = code.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((code, ""));
        let (class, target) = classify(mn, ops);
        body.push(Insn { text: code.to_string(), class, target });
    }
    flush(&in_hot, &mut body, &mut findings, &mut hot);
    (findings, hot)
}

/// `docs/gravity/asm/known-findings.txt`: one `kernel label finding` per line, the findings on
/// today's kernels that are reported, not fixed here (a stale line fails the lint too).
pub fn known_findings() -> Vec<String> {
    fs::read_to_string(corpus_dir().join("known-findings.txt")).map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect()).unwrap_or_default()
}
