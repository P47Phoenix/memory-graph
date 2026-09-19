use serde::{Deserialize, Serialize};

pub type NodeId = u64;

/// Hierarchy: Org > Repo > File > Symbol* > Token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Org,
    Repo,
    File,
    Symbol,
    Token,
}

impl NodeKind {
    /// Whether a CONTAINS edge `parent -> child` is valid.
    pub fn can_contain(self, child: NodeKind) -> bool {
        use NodeKind::*;
        matches!(
            (self, child),
            (Org, Repo)
                | (Repo, File)
                | (File, Symbol)
                | (File, Token)
                | (Symbol, Symbol)
                | (Symbol, Token)
        )
    }
}

/// Generic symbol kind vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Type,
    Function,
    Method,
    Variable,
    Constant,
    Other,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Type => "type",
            Self::Function => "function",
            Self::Method => "method",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for SymbolKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "module" => Self::Module,
            "type" => Self::Type,
            "function" => Self::Function,
            "method" => Self::Method,
            "variable" => Self::Variable,
            "constant" => Self::Constant,
            "other" => Self::Other,
            _ => return Err(format!("unknown symbol kind `{s}`")),
        })
    }
}

/// Token class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenClass {
    Identifier,
    Keyword,
    Literal,
    Operator,
    Punctuation,
    Comment,
    Other,
}

impl TokenClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::Keyword => "keyword",
            Self::Literal => "literal",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Comment => "comment",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for TokenClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "identifier" => Self::Identifier,
            "keyword" => Self::Keyword,
            "literal" => Self::Literal,
            "operator" => Self::Operator,
            "punctuation" => Self::Punctuation,
            "comment" => Self::Comment,
            "other" => Self::Other,
            _ => return Err(format!("unknown token class `{s}`")),
        })
    }
}

/// Source range: byte offsets `[start, end)`, 1-based lines, 1-based columns
/// counted in Unicode scalar values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// A token as produced by an extractor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenDecl {
    pub text: String,
    pub class: TokenClass,
    pub span: Span,
}

/// A stored node. Fields not relevant to a kind are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    /// Org/repo label, file path, symbol name or token text.
    pub name: String,
    /// File only: language string (`rust`, `zig`, `unknown`, ...).
    pub language: Option<String>,
    /// Symbol only.
    pub symbol_kind: Option<SymbolKind>,
    /// Symbol only: language-specific kind string (`struct`, `trait`, ...).
    pub lang_kind: Option<String>,
    /// Token only.
    pub token_class: Option<TokenClass>,
    /// File only: the extractor hit a syntax error and fell back to tokens only.
    #[serde(default)]
    pub has_errors: bool,
    /// Symbol and token.
    pub span: Option<Span>,
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("invalid containment: {0:?} cannot contain {1:?}")]
    InvalidContainment(NodeKind, NodeKind),
}

pub fn check_contains(parent: NodeKind, child: NodeKind) -> Result<(), SchemaError> {
    if parent.can_contain(child) {
        Ok(())
    } else {
        Err(SchemaError::InvalidContainment(parent, child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_rules() {
        use NodeKind::*;
        assert!(check_contains(Org, Repo).is_ok());
        assert!(check_contains(Symbol, Symbol).is_ok());
        assert!(check_contains(File, Token).is_ok());
        assert!(check_contains(Org, File).is_err());
        assert!(check_contains(Token, Token).is_err());
        assert!(check_contains(Repo, Symbol).is_err());
    }

    #[test]
    fn node_round_trip() {
        let n = Node {
            id: 7,
            parent: Some(3),
            kind: NodeKind::Symbol,
            name: "foo".into(),
            language: None,
            symbol_kind: Some(SymbolKind::Method),
            lang_kind: Some("fn".into()),
            token_class: None,
            has_errors: false,
            span: Some(Span {
                start: 0,
                end: 5,
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 6,
            }),
        };
        let j = serde_json::to_string(&n).unwrap();
        assert_eq!(serde_json::from_str::<Node>(&j).unwrap(), n);
        for k in [SymbolKind::Module, SymbolKind::Other] {
            assert_eq!(k.as_str().parse::<SymbolKind>().unwrap(), k);
        }
        assert_eq!(
            "comment".parse::<TokenClass>().unwrap(),
            TokenClass::Comment
        );
    }
}
