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

These rows carry the whole definition's span: text output prints `file:start_line:start_col-end_line:end_col`, and `--json` has `span` with byte offsets and line/col for both ends.

`--symbol-kind` narrows within the grain (`--grain class --symbol-kind struct` is the nearest enclosing struct; `--grain method --symbol-kind function` free functions only); a generic kind the grain can never hold (`--grain class --symbol-kind method`) is refused.

A hit with no enclosing symbol of that grain is rolled up to its file with `no_matching_symbol`; a file with no symbols at all (a language without an extractor) with `no_symbols`.

## Symbol patterns

`symbols` patterns: `name` (exact), `prefix*`, `*` (all), `name\*` (a literal `*`). `**` is rejected as ambiguous.

## Paging and JSON

Page with `--limit N --offset M`; results are ordered by org, repo, file, position. `--json` prints `{"query", "results": [...]}` (and `"grain"` for `search`).

## AI assistants (MCP)

`memory-graph --db /abs/path/graph.redb mcp` (or `--server host:port mcp`) serves the same queries as read-only MCP tools over stdio; `serve --mcp-listen 127.0.0.1:7071` adds an opt-in, loopback-only, unauthenticated streamable HTTP endpoint. Client configuration for Claude Code and Claude Desktop: [docs/mcp.md](../mcp.md).
