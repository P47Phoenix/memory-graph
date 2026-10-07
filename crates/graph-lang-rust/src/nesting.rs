//! A linear pre-scan that keeps `syn` off input nested too deep to parse
//! safely (#245). `syn` recurses once per delimiter, prefix operator and
//! generic level, and builds (then visits and drops) one boxed node per
//! link of a binary-operator or method chain, so a generated file such as
//! rustc's `tests/ui/parser/survive-peano-lesson-queue.rs` (2005 nested
//! parentheses) overflows a thread's stack. Such a file is stored tokens
//! only instead.
use graph_core::{TokenClass, TokenDecl};

/// Deepest nesting `syn` is asked to parse: open delimiters plus an
/// unbroken run of operators (`!!!!x`, `- - -x`) at that point. A release
/// build on a 2 MiB thread parses 255 nested parentheses (rustc's
/// `super-fast-paren-parsing.rs`) and overflows at 1000; extraction threads
/// get 16 MiB (`graph_core::EXTRACT_STACK_BYTES`).
pub const MAX_NESTING_DEPTH: usize = 256;

/// Longest chain `syn` is asked to parse: operator and `.` tokens in one
/// delimiter group between separators (`;`, `,`, `=>`, a closed `{..}`
/// block), such as `a + b + ...`, `x.a().b()...` or `Vec<Vec<...>>`. A
/// release build on a 2 MiB thread survives 3000 links and overflows at
/// 10000. Arrows (`->`, `=>`) and a lone `=` are not links.
pub const MAX_CHAIN_LEN: usize = 1024;

/// Why `tokens` are too deeply nested for `syn`, or `None` when it is safe
/// to parse them. Comments and literals never count. The shared tokenizer
/// emits one token per operator character, so `->` is `-`, `>`.
pub fn too_deep(tokens: &[TokenDecl]) -> Option<String> {
    // One chain counter per open delimiter group, plus the file level.
    let mut chains = vec![0usize];
    let mut run = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        i += 1;
        match t.class {
            TokenClass::Comment => continue,
            TokenClass::Operator => {}
            _ => run = 0,
        }
        let text = t.text.as_str();
        let arrow = matches!(text, "-" | "=")
            && tokens
                .get(i)
                .is_some_and(|n| n.text == ">" && n.span.start == t.span.end);
        if arrow {
            i += 1;
            run = 0;
            if text == "=" {
                reset(&mut chains); // a match arm
            }
            continue;
        }
        match text {
            "(" | "[" | "{" => chains.push(0),
            ")" | "]" | "}" => {
                if chains.len() > 1 {
                    chains.pop();
                }
                if text == "}" {
                    reset(&mut chains); // the end of an item, arm or block
                }
            }
            ";" | "," => reset(&mut chains),
            _ => {}
        }
        if t.class == TokenClass::Operator {
            run += 1;
            if chains.len() - 1 + run > MAX_NESTING_DEPTH {
                return Some(too_deep_why());
            }
        } else if chains.len() - 1 > MAX_NESTING_DEPTH {
            return Some(too_deep_why());
        }
        let link = (t.class == TokenClass::Operator && text != "=") || text == ".";
        if let Some(c) = chains.last_mut().filter(|_| link) {
            *c += 1;
            if *c > MAX_CHAIN_LEN {
                return Some(format!(
                    "skipped parsing: an expression chain exceeds {MAX_CHAIN_LEN} links (#245)"
                ));
            }
        }
    }
    None
}

fn too_deep_why() -> String {
    format!("skipped parsing: nesting depth exceeds {MAX_NESTING_DEPTH} (#245)")
}

fn reset(chains: &mut [usize]) {
    if let Some(c) = chains.last_mut() {
        *c = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::tokenizer::{tokenize_with, TokenizerOptions};

    fn scan(src: &str) -> Option<String> {
        let opts = TokenizerOptions {
            rust_literals: true,
            ..Default::default()
        };
        too_deep(&tokenize_with(src, opts))
    }

    fn parens(n: usize) -> String {
        format!("fn f() {{ {}1{}; }}", "(".repeat(n), ")".repeat(n))
    }

    #[test]
    fn ordinary_code_passes() {
        assert_eq!(
            scan("fn main() { let v = vec![(1, [2]), (3, [4])]; }"),
            None
        );
        assert_eq!(scan(""), None);
    }

    #[test]
    fn depth_limit_is_exact() {
        // `fn f() {` opens one group, the parentheses the rest.
        assert_eq!(scan(&parens(MAX_NESTING_DEPTH - 1)), None);
        let why = scan(&parens(MAX_NESTING_DEPTH)).expect("too deep");
        assert!(why.contains("nesting depth"), "{why}");
    }

    #[test]
    fn every_delimiter_kind_nests() {
        let n = MAX_NESTING_DEPTH + 1;
        for (o, c) in [("(", ")"), ("[", "]"), ("{", "}")] {
            let src = format!("{}{}", o.repeat(n), c.repeat(n));
            assert!(scan(&src).is_some(), "{o}");
        }
    }

    #[test]
    fn delimiters_in_comments_and_strings_do_not_count() {
        let n = MAX_NESTING_DEPTH * 2;
        let open = "(".repeat(n);
        assert_eq!(scan(&format!("// {open}\n/* {open} */ fn f() {{}}")), None);
        assert_eq!(scan(&format!("const S: &str = \"{open}\";")), None);
        assert_eq!(scan(&format!("const S: &str = r#\"{open}\"#;")), None);
    }

    #[test]
    fn closing_delimiters_unwind_and_never_underflow() {
        let wide = parens(100).repeat(50);
        assert_eq!(scan(&wide), None);
        assert_eq!(scan(&")".repeat(5000)), None);
    }

    #[test]
    fn prefix_operator_runs_nest() {
        let src = format!(
            "fn f() -> bool {{ {}true }}",
            "! ".repeat(MAX_NESTING_DEPTH + 1)
        );
        let why = scan(&src).expect("too deep");
        assert!(why.contains("nesting depth"), "{why}");
    }

    #[test]
    fn long_chains_are_caught_and_separators_reset_them() {
        let chain = |sep: &str, n: usize| vec!["1"; n].join(sep);
        let long = format!("fn f() {{ {} }}", chain(" + ", MAX_CHAIN_LEN + 2));
        assert!(scan(&long).expect("too long").contains("chain"));
        let calls = format!("fn f() {{ x{} }}", ".a()".repeat(MAX_CHAIN_LEN + 1));
        assert!(scan(&calls).is_some());
        // Many short statements and list items are not one chain.
        let stmts = "let a = 1 + 2;".repeat(MAX_CHAIN_LEN);
        assert_eq!(scan(&format!("fn f() {{ {stmts} }}")), None);
        let items = format!("const A: [u8; 3000] = [{}];", chain(", ", 3000));
        assert_eq!(scan(&items), None);
        // Nor are many items, match arms with block bodies, or bindings.
        let fns = "fn a(x: &'static u8) -> Vec<u8> { x.a() }
"
        .repeat(MAX_CHAIN_LEN);
        assert_eq!(scan(&fns), None);
        let arms = "X::A => { a.b() }
"
        .repeat(MAX_CHAIN_LEN * 2);
        assert_eq!(scan(&format!("fn f() {{ match x {{ {arms} }} }}")), None);
    }
}
