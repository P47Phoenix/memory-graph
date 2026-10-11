//! The symbol-name pattern language (ADR 0011 D2): one parser shared by the
//! embedded store, the server and the client, so they all agree on what a
//! pattern means.
//!
//! - `name` is an exact name; `prefix*` a prefix; `*` alone lists every
//!   symbol (an empty prefix).
//! - `*needle*` is an infix (substring) match and `*needle` a suffix match.
//!   The needle is 2 to 256 bytes ([`MIN_NEEDLE_BYTES`], [`MAX_NEEDLE_BYTES`])
//!   and may not contain an unescaped `*`.
//! - `\*` at either end is a literal `*`, not a marker: `foo\*` is the name
//!   `foo*`, `\*get*` the prefix `*get`. Inside an infix or suffix needle a
//!   `\*` is also a literal `*`; anywhere else `\` is a literal byte.
//! - Empty patterns, and patterns ending in `**`, are rejected.

use crate::StoreError;

/// The shortest infix or suffix needle, in bytes (ADR 0011 D3).
pub const MIN_NEEDLE_BYTES: usize = 2;
/// The longest infix or suffix needle, in bytes (ADR 0011 D3).
pub const MAX_NEEDLE_BYTES: usize = 256;

/// The server capability (`HelloResponse.capabilities`) that says it
/// understands infix and suffix patterns (ADR 0011 D6).
pub const CAPABILITY_SYMBOL_INFIX: &str = "symbol_infix";

/// A parsed symbol pattern. The strings are the name bytes to match, with
/// markers and escapes removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolPattern {
    Exact(String),
    Prefix(String),
    Infix(String),
    Suffix(String),
}

impl SymbolPattern {
    /// Whether this pattern needs a server that lists
    /// [`CAPABILITY_SYMBOL_INFIX`]: an older server reads `*get*` as the
    /// prefix `*get` and silently answers nothing.
    pub fn needs_infix_capability(&self) -> bool {
        matches!(self, SymbolPattern::Infix(_) | SymbolPattern::Suffix(_))
    }

    /// The same pattern with ASCII letters folded (ADR 0010 D4).
    pub fn folded(&self) -> SymbolPattern {
        let f = |s: &str| s.to_ascii_lowercase();
        match self {
            SymbolPattern::Exact(s) => SymbolPattern::Exact(f(s)),
            SymbolPattern::Prefix(s) => SymbolPattern::Prefix(f(s)),
            SymbolPattern::Infix(s) => SymbolPattern::Infix(f(s)),
            SymbolPattern::Suffix(s) => SymbolPattern::Suffix(f(s)),
        }
    }

    /// Whether `name` matches (as written: fold both sides first for a
    /// case-insensitive match).
    pub fn matches(&self, name: &str) -> bool {
        match self {
            SymbolPattern::Exact(s) => name == s,
            SymbolPattern::Prefix(s) => name.starts_with(s.as_str()),
            SymbolPattern::Infix(s) => name.contains(s.as_str()),
            SymbolPattern::Suffix(s) => name.ends_with(s.as_str()),
        }
    }
}

fn rejected(msg: impl Into<String>) -> StoreError {
    StoreError::Rejected(msg.into())
}

/// Parse a `symbols` pattern (ADR 0011 D2). Bad input is
/// [`StoreError::Rejected`]; this never panics.
pub fn parse_symbol_pattern(input: &str) -> Result<SymbolPattern, StoreError> {
    if input.is_empty() {
        return Err(rejected(
            "empty symbol pattern (use `*` to list all symbols)",
        ));
    }
    if input == "*" {
        return Ok(SymbolPattern::Prefix(String::new()));
    }
    if input.ends_with("**") {
        return Err(rejected(
            "pattern ending in `**` is ambiguous: use `prefix*` for a prefix, `*needle*` for \
             a name containing `needle` (no `*` inside it), or `name\\*` for a literal `*`",
        ));
    }
    let (lead_marker, lead_literal, rest) = if let Some(r) = input.strip_prefix("\\*") {
        (false, true, r)
    } else if let Some(r) = input.strip_prefix('*') {
        (true, false, r)
    } else {
        (false, false, input)
    };
    let (trail_marker, trail_literal, body) = if let Some(b) = rest.strip_suffix("\\*") {
        (false, true, b)
    } else if let Some(b) = rest.strip_suffix('*') {
        (true, false, b)
    } else {
        (false, false, rest)
    };
    if !lead_marker {
        // Exact or prefix: today's rules, the body used as written.
        let mut name = String::with_capacity(input.len());
        if lead_literal {
            name.push('*');
        }
        name.push_str(body);
        if trail_literal {
            name.push('*');
        }
        return Ok(if trail_marker {
            SymbolPattern::Prefix(name)
        } else {
            SymbolPattern::Exact(name)
        });
    }
    // Infix or suffix: `\*` inside the needle is a literal `*`; a bare `*`
    // is refused (no general globbing).
    let mut needle = String::with_capacity(body.len() + 1);
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'*') => {
                chars.next();
                needle.push('*');
            }
            '*' => {
                return Err(rejected(
                    "unescaped `*` inside an infix or suffix needle \
                     (only `*needle*` and `*needle` are supported; write `\\*` for a literal `*`)",
                ))
            }
            c => needle.push(c),
        }
    }
    if trail_literal {
        needle.push('*');
    }
    let what = if trail_marker { "infix" } else { "suffix" };
    if needle.is_empty() {
        return Err(rejected(format!("the {what} needle is empty")));
    }
    if needle.len() < MIN_NEEDLE_BYTES {
        return Err(rejected(format!(
            "{what} needle needs at least {MIN_NEEDLE_BYTES} bytes"
        )));
    }
    if needle.len() > MAX_NEEDLE_BYTES {
        return Err(rejected(format!(
            "{what} needle is longer than {MAX_NEEDLE_BYTES} bytes"
        )));
    }
    Ok(if trail_marker {
        SymbolPattern::Infix(needle)
    } else {
        SymbolPattern::Suffix(needle)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(s: &str) -> Result<SymbolPattern, String> {
        parse_symbol_pattern(s).map_err(|e| e.to_string())
    }
    use SymbolPattern::*;

    /// The ADR 0011 D2 table, row by row (`*` alone keeps today's meaning,
    /// list everything; see the module doc).
    #[test]
    fn the_adr_table() {
        assert!(p("").unwrap_err().contains("empty symbol pattern"));
        assert_eq!(p("*"), Ok(Prefix(String::new())));
        for bad in ["**", "***", "**get**"] {
            assert!(p(bad).unwrap_err().contains("ambiguous"), "{bad}");
        }
        assert!(p("*a*b*").unwrap_err().contains("unescaped `*`"));
        assert_eq!(p("*get*"), Ok(Infix("get".into())));
        assert_eq!(p("*get\\*"), Ok(Suffix("get*".into())));
        assert_eq!(p("\\*get*"), Ok(Prefix("*get".into())));
        assert_eq!(p("* get *"), Ok(Infix(" get ".into())));
        assert_eq!(p("g*"), Ok(Prefix("g".into())));
        assert!(p("*g*").unwrap_err().contains("at least 2 bytes"));
        assert_eq!(p("*é*"), Ok(Infix("é".into())));
        assert_eq!(p("*中*"), Ok(Infix("中".into())));
        // Kept from today.
        assert_eq!(p("foo\\*"), Ok(Exact("foo*".into())));
        assert_eq!(p("\\*"), Ok(Exact("*".into())));
        assert_eq!(p("foo\\"), Ok(Exact("foo\\".into())));
        assert_eq!(p("a*b"), Ok(Exact("a*b".into())));
        assert_eq!(p("a*b*"), Ok(Prefix("a*b".into())));
        assert!(p("foo\\**").unwrap_err().contains("ambiguous"));
        // Suffix and escapes inside a needle.
        assert_eq!(p("*get"), Ok(Suffix("get".into())));
        assert_eq!(p("*a\\*b*"), Ok(Infix("a*b".into())));
        assert!(p("*g")
            .unwrap_err()
            .contains("suffix needle needs at least 2"));
        assert!(p("*\\*").unwrap_err().contains("at least 2"));
        assert_eq!(p("*\\*\\*"), Ok(Suffix("**".into())));
    }

    #[test]
    fn needle_bounds_are_bytes_and_never_split() {
        let a = |n: usize| "a".repeat(n);
        assert_eq!(p(&format!("*{}*", a(255))), Ok(Infix(a(255))));
        assert_eq!(p(&format!("*{}*", a(256))), Ok(Infix(a(256))));
        assert!(p(&format!("*{}*", a(257)))
            .unwrap_err()
            .contains("longer than 256 bytes"));
        // `é` is 2 bytes: 255 + 2 = 257 straddles the limit and is refused
        // whole; 254 + 2 = 256 is accepted.
        assert!(p(&format!("*{}é*", a(255))).is_err());
        assert_eq!(
            p(&format!("*{}é*", a(254))),
            Ok(Infix(format!("{}é", a(254))))
        );
        assert!(p(&format!("*{}", a(257))).is_err());
    }

    proptest! {
        /// Arbitrary input never panics; anything accepted has a needle
        /// within bounds and round-trips its matching rule.
        #[test]
        fn the_parser_never_panics(s in "(\\PC|[*\\\\])*") {
            match parse_symbol_pattern(&s) {
                Ok(Infix(n)) | Ok(Suffix(n)) => {
                    prop_assert!((MIN_NEEDLE_BYTES..=MAX_NEEDLE_BYTES).contains(&n.len()));
                }
                Ok(_) => {}
                Err(e) => prop_assert!(matches!(e, StoreError::Rejected(_))),
            }
        }
    }
}
