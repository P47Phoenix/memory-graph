//! A stack big enough for `syn` on any input (#245).
//!
//! `syn` parses recursively, and the visit and the drop of its syntax tree
//! recurse too, once per nesting level. No shape check can enumerate every
//! way to nest (`break break ..`, `x as u8 as u8 ..`, `x[0][0]..`,
//! `fn() -> fn() -> ..`, closures, references, `else if` chains, ...), but
//! every nesting level consumes at least one token, and `syn` parses,
//! visits and drops a file's top-level items one after another, not nested.
//! So the stack a parse needs is bounded by
//! `STACK_BASE + largest_item * STACK_PER_TOKEN`, where `largest_item` is
//! the most non-comment tokens in one top-level item ([`largest_item`]) and
//! `STACK_PER_TOKEN` is the worst stack per token measured over the deepest
//! shapes we know, times at least 3.
//!
//! [`parse_on_safe_stack`] parses in place when that bound is under
//! 256 KiB, else on a dedicated thread of the bound's size (at least
//! 16 MiB), and skips `syn` (tokens only, with a note) only when the bound
//! exceeds [`MAX_PARSE_STACK`]. The thread is internal to the extractor, so
//! library callers of `RustExtractor::extract` get the same guarantee on
//! any thread that has 256 KiB of stack free. That is a requirement on the
//! caller: an in-place parse (bound under 256 KiB) runs on the caller's own
//! thread and needs 256 KiB of free stack there, so a library caller that
//! runs extraction on a small custom stack must leave at least that much.
//!
//! A dedicated thread's stack stays committed, as far as it was touched,
//! until the thread exits, and parse jobs run in parallel (#254). Before it
//! spawns one, the parse reserves the stack it may touch ([`touched`]: the
//! bound over its 3x margin) from the caller's memory budget through
//! [`graph_core::reserve_scratch`], and gives it back once the thread has
//! been joined. The CLI installs its `--memory` ingest budget there, so
//! parallel near-cap files wait for each other instead of exceeding it.
//!
//! The tokenizer's token count is never less than the tokens `syn` sees:
//! it splits multi-character operators (`&&`, `->`) that `syn` reads as one.
//!
//! Measured with `examples/stack_probe.rs` (Windows x86_64, rustc stable,
//! 2026-10-06): the smallest thread stack that survives each shape, over
//! its non-comment token count (about 20k-140k tokens in release, 4k-28k in
//! debug). The worst shapes, in bytes per token:
//!
//! | shape (repeated) | release B/token | debug B/token | debug, syn opt-level 0 |
//! |---|---|---|---|
//! | `break ` (keyword-led) | 3,878 | 3,794 | 11,930 |
//! | `return ` | 2,804 | 2,699 | 10,301 |
//! | `V<` .. `>` (generics) | 2,796 | 2,664 | 17,190 |
//! | `{ ` .. `}` (blocks) | 2,726 | 2,734 | 11,049 |
//! | `loop { ` / `unsafe { ` | 1,835 | 1,846 | |
//! | `(` .. `)`, `[` .. `]`, `*const ` | 1,691-1,704 | 1,638-1,671 | 6,930 (parens) |
//! | `|a| ` (closures), `!`, `-`, `*` | 1,494-1,507 | 1,442 | |
//! | `mod m { `, `fn f() { ` | 1,285-1,442 | 1,290-1,451 | 11,537 (`fn`) |
//! | `fn() -> `, `&'a ` | 1,122-1,127 | 1,082-1,104 | |
//! | `a = ` | 1,009 | 992 | |
//! | `as u8`, `[0]`, `.a()`, `+`, `?`, `.await`, `else if` | 13-351 | 151-755 | |
//! | one-line file (base) | 20 KB | 20 KB | 68 KB |
//!
//! "Debug" is this workspace's dev profile, which builds `syn` optimized
//! (`[profile.dev.package."*"] opt-level = 3`), so it matches release. The
//! last column builds `syn` at opt-level 0, as a library consumer's debug
//! build might.
//!
//! A large reservation costs address space only. Linux and macOS commit
//! stack pages as they are touched; Windows reserves the whole stack and
//! charges only touched pages against the commit limit. All shipped
//! targets are 64-bit.
use graph_core::{SymbolDecl, TokenClass, TokenDecl};

/// Worst measured stack per non-comment token, times at least 3: release
/// 12 KiB (worst 3,878 B, `break` chains); debug 52 KiB, sized for a `syn`
/// built at opt-level 0 (worst 17.2 KB, nested generics), although this
/// workspace's dev profile optimizes dependencies.
pub const STACK_PER_TOKEN: usize = if cfg!(debug_assertions) {
    52 << 10
} else {
    12 << 10
};

/// Stack a parse needs whatever its input (the frames above the first
/// nesting level), measured on a one-line file, times at least 3: release
/// 64 KiB (20 KB measured), debug 208 KiB (68 KB with `syn` at opt-level 0).
pub const STACK_BASE: usize = if cfg!(debug_assertions) {
    208 << 10
} else {
    64 << 10
};

/// Largest parse stack the extractor reserves: 2 GiB, enough for a
/// top-level item of 174,757 tokens in a release build (40,325 in debug). A
/// file with a larger item is stored tokens only.
pub const MAX_PARSE_STACK: usize = 2 << 30;

/// Parse in place when the bound fits in this much stack.
const IN_PLACE_STACK: usize = 256 << 10;

/// Smallest dedicated parse thread.
const MIN_THREAD_STACK: usize = graph_core::EXTRACT_STACK_BYTES;

/// Keywords that can only start an item: after a top-level `}`, one of
/// these (or `#`, or a macro invocation `name!`) begins the next item.
const ITEM_START: &[&str] = &[
    "async",
    "const",
    "enum",
    "extern",
    "fn",
    "impl",
    "macro_rules",
    "mod",
    "pub",
    "static",
    "struct",
    "trait",
    "type",
    "union",
    "unsafe",
    "use",
];

/// The most non-comment tokens in one top-level item of `tokens`. An item
/// ends at a `;` outside every delimiter, or at a `}` closing back to the
/// top level when an item start follows. Splitting anywhere else (inside
/// an item) could under-count, so a `}` followed by anything else, such as
/// `const C: u8 = {1} + {2};`, does not split.
pub(crate) fn largest_item(tokens: &[TokenDecl]) -> usize {
    let code: Vec<&TokenDecl> = tokens
        .iter()
        .filter(|t| t.class != TokenClass::Comment)
        .collect();
    let (mut depth, mut item, mut largest) = (0usize, 0usize, 0usize);
    for (i, t) in code.iter().enumerate() {
        item += 1;
        let text = t.text.as_str();
        // Angle brackets are not tracked: `<` and `>` are also comparison
        // and shift operators, which tokens alone cannot tell apart. That is
        // sound because a `}` inside `<...>` (a const generic block such as
        // `T<{ N }>`) is, in valid code, always followed by `,` or `>`, never
        // by `#`, an item keyword or `name!`, so it never ends an item early
        // and never under-counts one.
        match text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            _ => {}
        }
        let ends = depth == 0
            && match text {
                ";" => true,
                "}" => code.get(i + 1).is_none_or(|n| {
                    n.text == "#"
                        || ITEM_START.contains(&n.text.as_str())
                        || code.get(i + 2).is_some_and(|b| b.text == "!")
                }),
                _ => false,
            };
        if ends {
            largest = largest.max(item);
            item = 0;
        }
    }
    largest.max(item)
}

/// The stack bound for a parse whose largest item has `item_tokens` tokens.
pub(crate) fn bound(item_tokens: usize) -> usize {
    item_tokens
        .saturating_mul(STACK_PER_TOKEN)
        .saturating_add(STACK_BASE)
}

/// The stack a parse thread of `size` bytes may actually touch: the size
/// over the 3x margin built into [`STACK_PER_TOKEN`] and [`STACK_BASE`].
/// This is what it reserves from the caller's memory budget (#254).
pub fn touched(size: usize) -> u64 {
    (size / 3) as u64
}

/// Where a parse runs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    InPlace,
    Thread(usize),
    TooBig(usize),
}

pub(crate) fn plan(item_tokens: usize) -> Plan {
    match bound(item_tokens) {
        b if b <= IN_PLACE_STACK => Plan::InPlace,
        b if b <= MAX_PARSE_STACK => Plan::Thread(b.max(MIN_THREAD_STACK)),
        b => Plan::TooBig(b),
    }
}

/// [`crate::parse_symbols_unguarded`] on a stack its bound fits:
/// `Ok(None)` for a syntax error, `Err(note)` when `syn` was skipped.
pub(crate) fn parse_on_safe_stack(
    source: &str,
    tokens: &[TokenDecl],
) -> Result<Option<Vec<SymbolDecl>>, String> {
    let item = largest_item(tokens);
    let size = match plan(item) {
        Plan::InPlace => return Ok(crate::parse_symbols_unguarded(source)),
        Plan::Thread(size) => size,
        Plan::TooBig(b) => {
            return Err(format!(
                "skipped parsing: a top-level item of {item} tokens needs up to {} MiB of parser stack, over the {} MiB cap (#245)",
                b >> 20,
                MAX_PARSE_STACK >> 20
            ))
        }
    };
    // Held until the thread has been joined, when its stack is unmapped.
    let _reserved = graph_core::reserve_scratch(touched(size));
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("rust-parse".into())
            .stack_size(size)
            .spawn_scoped(s, || crate::parse_symbols_unguarded(source))
            .map_err(|e| {
                format!(
                    "skipped parsing: could not start a {} MiB parse thread: {e} (#245)",
                    size >> 20
                )
            })?
            .join()
            .map_err(|_| "skipped parsing: the parser panicked (#245)".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::tokenizer::{tokenize_with, TokenizerOptions};

    fn largest(src: &str) -> usize {
        let opts = TokenizerOptions {
            rust_literals: true,
            ..Default::default()
        };
        largest_item(&tokenize_with(src, opts))
    }

    #[test]
    fn items_split_at_top_level_ends() {
        assert_eq!(largest(""), 0);
        // `fn a ( ) { }` is 6 tokens; `use a ;` is 3; comments do not count.
        assert_eq!(largest("fn a() {}\n// x y z\nuse a;\nfn b() {}"), 6);
        assert_eq!(largest("struct A; struct B;"), 3);
        assert_eq!(largest("#[a] fn a() {} m! { x } fn b() {}"), 10);
    }

    #[test]
    fn blocks_inside_an_item_do_not_split_it() {
        // An expression continuing after a top-level `}` stays one item.
        assert_eq!(largest("const C: u8 = {1} + {2};"), 13);
        assert_eq!(largest("fn f() { {} {} }"), 10);
        assert_eq!(largest("const C: [u8; 2] = [0; 2];"), 15);
    }

    #[test]
    fn plan_boundaries() {
        assert_eq!(plan(0), Plan::InPlace);
        let k = STACK_PER_TOKEN;
        let in_place = (IN_PLACE_STACK - STACK_BASE) / k;
        assert_eq!(plan(in_place), Plan::InPlace);
        assert_eq!(plan(in_place + 1), Plan::Thread(MIN_THREAD_STACK));
        let cap = (MAX_PARSE_STACK - STACK_BASE) / k;
        assert_eq!(plan(cap), Plan::Thread(bound(cap)));
        assert!(matches!(plan(cap + 1), Plan::TooBig(_)));
        assert!(matches!(plan(usize::MAX), Plan::TooBig(_)));
    }
}
