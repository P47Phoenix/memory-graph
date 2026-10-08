# Querying

`search` finds **tokens** by exact text; `symbols` finds **definitions** by name. Both work the same on a `--db` file and over `--server` ([server guide](server.md)).

## `--kind` means two things

`--kind` means a different thing on each command:

- on `search`, a token class: identifier, keyword, literal, operator, punctuation, comment, other;
- on `symbols`, a symbol kind: generic (module, type, function, method, variable, constant, other) or language-specific (struct, trait, impl, ...).

`--language`, `--kind` and `--symbol-kind` values are validated against what `describe` reports, so a typo is an error, not an empty result. Language names are case-insensitive.

## Grains: rolling hits up

`search --grain token|symbol|method|class|file|repo|org` rolls hits up to that level.

- `symbol` is the nearest enclosing symbol of any kind.
- `method` is the nearest enclosing method or free function.
- `class` is the nearest enclosing type (struct, class, interface, trait, enum, ...) or Rust `impl` block, or, when nothing type-like encloses the hit, the same-file type named by the enclosing symbol's `owner` (a Go method's receiver type).

These rows carry the whole definition's span: text output prints `file:name_line:name_col-end_line:end_col`, from the declaration position (below) to the end, and `--json` has `span` with byte offsets and line/col for both ends, plus `name_line`, `name_col` and `name_pos`.

## Declaration position

A symbol's span covers everything that belongs to it, including attributes, decorators and annotations above it, so for `[Serializable]` on line 1 and `public class Shape { }` on line 2 the span starts on line 1. The **declaration position** is where the name is: the first identifier token inside the span whose text equals the symbol's name, or the span start if there is none. It is worked out at query time from the stored tokens, the same way for every language.

- `symbols` text output prints `file:name_line:name_col` (`Shape.cs:2:14` above); `--json` keeps the full `span` and adds `name_line`, `name_col` and `name_pos` (`{byte, line, col}`).
- The rule is a plain first match: when the name also appears as an identifier inside an attribute (`[Shape] class Shape`, `[Foo(Shape)] class Shape`), the attribute's line is reported. A string literal (`[Alias("Shape")]`) is not an identifier and does not count.
- A server from before this change does not send it; the client then uses the span start.

`--symbol-kind` narrows within the grain (`--grain class --symbol-kind struct` is the nearest enclosing struct; `--grain method --symbol-kind function` free functions only); a generic kind the grain can never hold (`--grain class --symbol-kind method`) is refused.

A hit with no enclosing symbol of that grain is rolled up to its file with `no_matching_symbol`; a file with no symbols at all (a language without an extractor) with `no_symbols`.

## Symbol patterns

`symbols` patterns: `name` (exact), `prefix*`, `*` (all), `name\*` (a literal `*`). `**` is rejected as ambiguous.

## Paging and JSON

Page with `--limit N --offset M`; results are ordered by org, repo, file, position. `--json` prints `{"query", "results": [...]}` (and `"grain"` for `search`).

## AI assistants (MCP)

`memory-graph --db /abs/path/graph.redb mcp` (or `--server host:port mcp`) serves the same queries as read-only MCP tools over stdio; `serve --mcp-listen 127.0.0.1:7071` adds an opt-in, loopback-only, unauthenticated streamable HTTP endpoint. Client configuration for Claude Code and Claude Desktop: [docs/mcp.md](../mcp.md).
