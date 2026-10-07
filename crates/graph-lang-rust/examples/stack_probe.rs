//! Measures the stack `syn` needs per token on deeply nested shapes (#245),
//! the numbers behind `STACK_PER_TOKEN` and `STACK_BASE` in `src/stack.rs`.
//!
//! ```sh
//! cargo run --release -p graph-lang-rust --example stack_probe -- measure 20000
//! cargo run -p graph-lang-rust --example stack_probe -- measure 5000
//! ```
//!
//! For every shape it binary-searches, in child processes (an overflow
//! aborts the process), the smallest thread stack on which
//! `parse_symbols_unguarded` survives, and prints it per non-comment token.
use graph_core::tokenizer::{tokenize_with, TokenizerOptions};
use graph_core::TokenClass;
use std::process::Command;

const SHAPES: &[&str] = &[
    "tiny", "paren", "not", "neg", "deref", "ref", "break", "return", "as", "index", "call",
    "field", "try", "await", "plus", "assign", "elseif", "closure", "block", "unsafe", "loop",
    "match", "array", "callnest", "macro", "pat", "fntype", "refty", "ptrty", "generic", "tuplety",
    "slicety", "path", "mod", "fnnest",
];

fn rep(s: &str, n: usize) -> String {
    s.repeat(n)
}

fn shape(name: &str, n: usize) -> String {
    let e = |body: String| format!("async fn f() {{ loop {{ {body}; }} }}\n");
    let t = |ty: String| format!("type T<'a> = {ty};\n");
    match name {
        "tiny" => "fn f() {}\n".into(),
        "paren" => e(format!("{}1{}", rep("(", n), rep(")", n))),
        "not" => e(format!("{}true", rep("!", n))),
        "neg" => e(format!("{}1", rep("-", n))),
        "deref" => e(format!("{}x", rep("*", n))),
        "ref" => e(format!("{}x", rep("& ", n))),
        "break" => e(format!("{}1", rep("break ", n))),
        "return" => e(format!("{}1", rep("return ", n))),
        "as" => e(format!("x{}", rep(" as u8", n))),
        "index" => e(format!("x{}", rep("[0]", n))),
        "call" => e(format!("x{}", rep(".a()", n))),
        "field" => e(format!("x{}", rep(".a", n))),
        "try" => e(format!("x{}", rep("?", n))),
        "await" => e(format!("x{}", rep(".await", n))),
        "plus" => e(vec!["1"; n].join(" + ")),
        "assign" => e(format!("{}1", rep("a = ", n))),
        "elseif" => e(format!("if a {{}} {}", rep("else if a {} ", n))),
        "closure" => e(format!("{}1", rep("|a| ", n))),
        "block" => e(format!("{}{}", rep("{ ", n), rep("}", n))),
        "unsafe" => e(format!("{}{}", rep("unsafe { ", n), rep("}", n))),
        "loop" => e(format!("{}{}", rep("loop { ", n), rep("}", n))),
        "match" => e(format!("{}1{}", rep("match x { _ => ", n), rep(" }", n))),
        "array" => e(format!("{}1{}", rep("[", n), rep("]", n))),
        "callnest" => e(format!("{}{}", rep("f(", n), rep(")", n))),
        "macro" => e(format!("m!{}{}", rep("(", n), rep(")", n))),
        "pat" => e(format!("let {}a{} = 1", rep("(", n), rep(")", n))),
        "fntype" => t(format!("{}u8", rep("fn() -> ", n))),
        "refty" => t(format!("{}u8", rep("&'a ", n))),
        "ptrty" => t(format!("{}u8", rep("*const ", n))),
        "generic" => t(format!("{}u8{}", rep("V<", n), rep(">", n))),
        "tuplety" => t(format!("{}u8{}", rep("(", n), rep(",)", n))),
        "slicety" => t(format!("{}u8{}", rep("[", n), rep("]", n))),
        "path" => t(format!("{}b", rep("a::", n))),
        "mod" => format!("{}{}", rep("mod m { ", n), rep("}", n)),
        "fnnest" => format!("{}{}", rep("fn f() { ", n), rep("}", n)),
        _ => panic!("unknown shape {name}"),
    }
}

fn code_tokens(src: &str) -> usize {
    let opts = TokenizerOptions {
        rust_literals: true,
        ..Default::default()
    };
    tokenize_with(src, opts)
        .iter()
        .filter(|t| t.class != TokenClass::Comment)
        .count()
}

fn run(name: &str, n: usize, stack: usize) {
    let src = shape(name, n);
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            std::hint::black_box(graph_lang_rust::parse_symbols_unguarded(&src));
        })
        .expect("spawn")
        .join()
        .expect("join");
}

fn survives(name: &str, n: usize, stack: usize) -> bool {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args(["run", name, &n.to_string(), &stack.to_string()])
        .output()
        .expect("child")
        .status
        .success()
}

fn measure(n: usize) {
    println!("| shape | tokens | min stack (bytes) | bytes/token |");
    println!("|---|---|---|---|");
    for name in SHAPES {
        let n = if *name == "tiny" { 1 } else { n };
        let tokens = code_tokens(&shape(name, n));
        let (mut lo, mut hi) = (16usize << 10, 4usize << 30);
        if !survives(name, n, hi) {
            println!("| {name} | {tokens} | > {hi} | n/a |");
            continue;
        }
        while hi - lo > (4 << 10).max(hi / 100) {
            let mid = lo + (hi - lo) / 2;
            if survives(name, n, mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        println!(
            "| {name} | {tokens} | {hi} | {:.0} |",
            hi as f64 / tokens as f64
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["run", name, n, stack] => run(name, n.parse().unwrap(), stack.parse().unwrap()),
        ["measure", n] => measure(n.parse().unwrap()),
        _ => eprintln!("usage: stack_probe measure <reps> | run <shape> <reps> <stack>"),
    }
}
