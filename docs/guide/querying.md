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

These rows carry the whole definition's span: text output prints `file:start_line:start_col-end_line:end_col`, and `--json` has `span` with byte offsets and line/col for both ends, plus `name_line`, `name_col` and `name_pos`.

## Declaration position

A symbol's span covers everything that belongs to it, including attributes, decorators and annotations above it, so for `[Serializable]` on line 1 and `public class Shape { }` on line 2 the span starts on line 1. The **declaration position** is where the name is: the first identifier token inside the span whose text equals the symbol's name, or the span start if there is none. It is worked out at query time from the stored tokens, the same way for every language.

- `symbols` text output prints `file:name_line:name_col` (`Shape.cs:2:14` above). `search` text output keeps the span range. `--json` of both (symbol, method and class grains for `search`) keeps the full `span` and adds `name_line`, `name_col` and `name_pos` (`{byte, line, col}`).
- A name that is not one identifier token with the same text (C++ `operator+`, or a name stored in another case by a case-insensitive language such as COBOL, SQL or RPG) is not found, so the span start is used.
- The rule is a plain first match: when the name also appears as an identifier inside an attribute (`[Shape] class Shape`, `[Foo(Shape)] class Shape`), the attribute's line is reported. A string literal (`[Alias("Shape")]`) is not an identifier and does not count.
- A server from before this change does not send it; the client then uses the span start.

`--symbol-kind` narrows within the grain (`--grain class --symbol-kind struct` is the nearest enclosing struct; `--grain method --symbol-kind function` free functions only); a generic kind the grain can never hold (`--grain class --symbol-kind method`) is refused.

A hit with no enclosing symbol of that grain is rolled up to its file with `no_matching_symbol`; a file with no symbols at all (a language without an extractor) with `no_symbols`.

## What matches: whole tokens, prefixes, infix

`search` matches whole tokens only; `symbols` can also match inside a name:

- `search <TEXT>` matches **whole tokens exactly**, case included. `search Gadget` finds the identifier `Gadget`; `search Gad`, `search gadget` and `search Gad*` do not (there is no wildcard on `search`).
- `symbols <PATTERN>` matches a **whole name**, or a **prefix** with a trailing `*`. It is case-insensitive by default; `--exact-case` makes it case-sensitive ([case](#case)).
- `symbols '*needle*'` matches a name that **contains** `needle` anywhere (infix), and `symbols '*needle'` a name that **ends** with it (suffix). Folded like every symbol lookup unless `--exact-case`. A bare fragment without the stars (`symbols Get`) is still an exact name and finds nothing inside `Gadget` ([ADR 0011](../adr/0011-infix-search.md)).
- `search` has **no infix match**: a token-level substring index would not fit the size budget, so `search Gad` finds nothing.

Worked example, a file `Gadget.cs` containing `public class Gadget { }`:

| Command | Finds `Gadget`? | Why |
|---|---|---|
| `symbols Gadget` | yes | exact name |
| `symbols gadget` | yes | case-insensitive by default |
| `symbols Gad*` | yes | prefix |
| `symbols gad*` | yes | prefix, case-insensitive by default |
| `symbols --exact-case Gad*` | yes | prefix, same case |
| `symbols --exact-case gad*` | no | the case differs |
| `symbols '*'` | yes | `*` alone lists every symbol |
| `symbols Get` | no | `Get` is not the name and not a prefix of it; write `'*get*'` for an infix match |
| `symbols '*get*'` | yes | infix: the name contains `get` (case-insensitive by default) |
| `symbols --exact-case '*Get*'` | no | infix, but the case differs |
| `symbols dget` | no | a fragment from inside the name |
| `symbols '*dget'` | yes | suffix: the name ends with `dget` |
| `search Gadget` | yes (the token) | whole token, exact text |
| `search Gad` | no | part of a token |
| `search gadget` | no | `search` is case-sensitive |
| `search Gad*` | no | `search` has no wildcard |

An empty answer therefore means "no name, prefix or infix matched", not "not indexed": `describe` shows what a repo holds, and `symbols '*' --file <path>` lists every symbol in one file.

## Symbol patterns

`symbols` patterns, parsed the same way by the embedded store, the server and the client:

| Pattern | Meaning |
|---|---|
| `name` | exact name |
| `prefix*` | names starting with `prefix` (no minimum length) |
| `*` | every symbol |
| `*needle*` | names containing `needle` (infix) |
| `*needle` | names ending with `needle` (suffix) |
| `name\*` | the name `name*` (a trailing `\*` is a literal `*`) |
| `\*name*` | names starting with `*name` (a leading `\*` is a literal `*`, not a marker) |

Quote any pattern with a `*` so the shell does not expand it. Rules for infix and suffix needles:

- The needle is **2 to 256 bytes** (bytes, not characters: `*é*` is 2 bytes and accepted, `*g*` is refused). A longer or shorter needle is refused, never cut.
- It may not contain an unescaped `*` (`*a*b*` is refused; there is no general globbing). `\*` inside it is a literal `*`. Whitespace is part of the needle.
- ASCII letters fold unless `--exact-case`; other characters match exactly ([case](#case)).
- Results come in the same order as any symbol query (org, repo, file, position), and `--limit`/`--offset` page them as usual.
- The first infix or suffix query on an open store walks every distinct symbol name and keeps them in memory (story 70); later queries, until the next write, scan that copy instead, which takes a few milliseconds. A one-shot CLI command pays the walk every time; `memory-graph serve` and `memory-graph mcp` keep the copy. It is bounded at 256 MiB per case mode (about 100 MB on an 877M-token index), and a store whose names do not fit always walks. A page decodes only the files it returns (the other candidate files are only sorted on a compact key), so a very common needle costs about what a prefix query costs. Over `--server` a deadline the client sets (`grpc-timeout`) stops it with `DeadlineExceeded`, never a partial page.
- An older server would read `*get*` as the prefix `*get` and answer nothing, so the client sends an infix or suffix pattern only to a server whose `Hello` lists the `symbol_infix` capability, and otherwise fails with "this server does not support infix symbol search; upgrade it".

**Changed with infix search (story 68).** Patterns that meant something else before:

- `*foo` is now a **suffix** query (names ending in `foo`). It used to look for a name literally starting with `*`; write `\*foo` for that now.
- `\*get*` is now the **prefix** `*get` (a leading `\*` is a literal `*`). It used to be the prefix `\*get`, backslash included.
- `\*\*` is now the exact name `**`. It used to be the exact name `\**`.
- A symbol query stopped by its deadline (`DeadlineExceeded`, see above) is an ordinary error: the CLI prints it and exits with code 1, with no rows printed. Over `--server`, the client sends `--read-deadline` as the request's deadline.

Refused: an empty pattern, any pattern ending in `**` (ambiguous; so `**`, `***` and `a\**`), and the infix/suffix cases above.

### Case

Symbol names match **case-insensitively by default**: `symbols widget` finds `Widget`, and so do `WIDGET` and `WID*`. Only ASCII letters fold (`A`-`Z` to `a`-`z`); every other character matches exactly, so `grö*` finds `Größe` while `GRÖ*` does not. Results come in the usual order (org, repo, file, position), each symbol once. `--exact-case` restores case-sensitive matching (`symbols --exact-case widget` finds nothing). The literal `name\*` form works in both modes. The language and kind filters were already ASCII-case-insensitive.

The lookup uses a folded index, `sym_fold`, kept beside the exact one. A database written by an older build gets it on its first open by this one: the open rebuilds it from the exact index, once, and prints a line saying so; later opens write nothing. Over `--server`, an older server ignores `--exact-case`'s field and answers case-sensitively, and an older client talking to a new server gets case-insensitive results, because it cannot send the field and its default is `false` ([ADR 0010](../adr/0010-symbol-lookup-and-positions.md) D4).

## Paging and JSON

Page with `--limit N --offset M`; results are ordered by org, repo, file, position. `--json` prints `{"query", "results": [...]}` (and `"grain"` for `search`).

## AI assistants (MCP)

`memory-graph --db /abs/path/graph.redb mcp` (or `--server host:port mcp`) serves the same queries as read-only MCP tools over stdio; `serve --mcp-listen 127.0.0.1:7071` adds an opt-in, loopback-only, unauthenticated streamable HTTP endpoint. Client configuration for Claude Code and Claude Desktop: [docs/mcp.md](../mcp.md).
