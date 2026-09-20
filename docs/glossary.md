# Glossary

Plain-language meanings for the words used in the docs. Terms are in alphabetical order. Each entry ends with links to where the term is used.

New here? Read the "In plain words" box at the top of each ADR first, and come back to this page when a word is unclear.

## ADR
An **Architecture Decision Record**: a short document that says "we chose X, here is why, here is what it costs". Like a diary entry for a big choice, so nobody has to guess later. Used in: [ADR 0001](adr/0001-storage.md), [ADR 0002](adr/0002-parsing-and-crate-layout.md), [ADR 0003](adr/0003-data-model.md), [docs index](README.md).

## ADR story / epic story (story numbering)
A "story" is one small piece of planned work. There are two numbering schemes, so always check which one is meant:
- **Epic story N** (for example "epic story 13") is a story in the [epic](epic-code-memory-graph.md), numbered 1-19.
- **ADR story N** (for example "ADR story 4") is a step in the build plan inside [ADR 0003](adr/0003-data-model.md), numbered from 0.

Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md), [ADR 0003](adr/0003-data-model.md).

## Commit / transaction
A **transaction** is a group of changes that either all happen or none happen, like paying at a till: the money and the receipt go together. A **commit** is the moment the group becomes permanent. Used in: [ADR 0003](adr/0003-data-model.md).

## Content hash / fingerprint
A **content hash** is a short code computed from a file's bytes. Change one letter and the code changes. A **fingerprint** in this project is that hash plus the language, the extractor version and the fingerprint format version, joined in one string. If the stored fingerprint matches the new one, the file has not changed and we skip it. Used in: [ADR 0001](adr/0001-storage.md), [learnings](learnings.md).

## Crate
Rust's word for a package of code. Think of it as one box of a bigger toy set. Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md).

## C dependency / pure Rust
**Pure Rust** means all our code and the libraries we use are written in Rust only. A **C dependency** is a library written in the C language that gets built or linked in. We ban them so the project builds anywhere with just the Rust tools. A CI check script enforces this. Used in: [ADR 0001](adr/0001-storage.md), [ADR 0002](adr/0002-parsing-and-crate-layout.md), [learnings](learnings.md).

## Dictionary
A table that gives each distinct piece of text one small number (its id). Like a school register: instead of writing "Alexandra Petrovna" everywhere, you write "17". Used in: [ADR 0003](adr/0003-data-model.md).

## Differential test
A test that runs the old code and the new code on the same input and checks they give the same answer. Used in: [ADR 0003](adr/0003-data-model.md).

## Epoch (commit epoch)
A counter that goes up by one on every commit. It lets us tell which version of a file is newer. Used in: [ADR 0003](adr/0003-data-model.md).

## Extractor
A small piece of code that reads one source file and reports what is in it: the symbols and the tokens, each with its span. There is a rich one for Rust and a general one for all other languages (see fallback tokenizer). Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md).

## Fallback tokenizer
The simple splitter that works for any language. It cuts text into tokens without understanding the language, so it finds no symbols. It is the "works for everything, but shallow" option. Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md).

## File / repo / org
The three levels above a symbol. A **file** is one source file. A **repo** (repository) is a project folder tracked by git. An **org** (organization) is the group that owns several repos. Like: org = a school, repo = a class, file = a student's notebook. Used in: [ADR 0001](adr/0001-storage.md), [ADR 0003](adr/0003-data-model.md).

## fsync
An operating-system call that forces data out of memory onto the disk, so it survives a power cut. Used in: [ADR 0003](adr/0003-data-model.md).

## Graph / node / parent
A **graph** is a set of things (**nodes**) plus links between them. Here, the nodes are org, repo, file, symbol and token. A node's **parent** is the thing that contains it: a file's parent is its repo. Like a family tree. Used in: [ADR 0001](adr/0001-storage.md), [ADR 0003](adr/0003-data-model.md).

## Grain / roll-up
The **grain** is the level you count at: token, symbol, file, repo or org. A **roll-up** adds up hits from small things into bigger things. Example: "the word `foo` appears 40 times in this repo" is a roll-up to repo grain. Used in: [ADR 0003](adr/0003-data-model.md), [learnings](learnings.md).

## Index / indexing
To **index** is to read source files and store what we found so we can search fast later. An **index** (the result) is like the one at the back of a book. Used in: [ADR 0001](adr/0001-storage.md).

## Inverted index / postings
An **inverted index** is a lookup from a word to the places it appears, like a book index. Each entry in it is a **posting**. A **count posting** stores only how many times a word appears in a file, not where. Used in: [ADR 0003](adr/0003-data-model.md).

## Labels [M] and [E]
Tags on numbers in the docs. **[M]** means measured: we ran something and saw this. **[E]** means estimated: a calculated guess, not yet measured. Used in: [learnings](learnings.md), [ADR 0003](adr/0003-data-model.md).

## MCP
**Model Context Protocol**: a standard way for AI assistants to call outside tools. Here it means an AI agent could search this database. Used in: [ADR 0003](adr/0003-data-model.md).

## Manifest
A small catalog file that lists all the shards and the version each one is at. Used in: [ADR 0003](adr/0003-data-model.md).

## MVCC
**Multi-version concurrency control**: readers keep seeing the data as it was when they started, while a writer makes new changes. Like reading a printed copy of a page while someone edits the original. Used in: [ADR 0003](adr/0003-data-model.md).

## NDJSON
**Newline-delimited JSON**: a text format with one JSON record per line. Tools use it to hand us ready-made symbols and tokens for any language. Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md).

## Page slack
Wasted space inside the fixed-size blocks ("pages") a database stores data in. Like half-empty boxes in a moving van. Used in: [ADR 0003](adr/0003-data-model.md), [learnings](learnings.md).

## proptest / property test
A test that makes many random inputs and checks that a rule always holds (for example "saving then loading gives back the same thing"). Used in: [ADR 0003](adr/0003-data-model.md).

## redb
The pure-Rust database library we use. It stores data in one file, supports transactions, and lets only one process open the file at a time (an exclusive lock). Used in: [ADR 0001](adr/0001-storage.md), [learnings](learnings.md).

## Shard
One slice of a database that is split up so it can grow bigger. Like several filing cabinets instead of one. In this project one shard is one redb file. Used in: [ADR 0003](adr/0003-data-model.md).

## Snapshot / snapshot isolation
A **snapshot** is a frozen view of the data at one moment. **Snapshot isolation** means a reader keeps seeing that frozen view even while a writer changes the database. Used in: [ADR 0003](adr/0003-data-model.md), [learnings](learnings.md).

## Span
The exact place in a file where something sits: start and end byte offsets, plus line and column. Like "page 3, line 5, letters 2 to 9". Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md), [ADR 0003](adr/0003-data-model.md).

## Stop-list
A list of very common words (like `(`) that we would skip storing. It is considered but not part of the decision. Used in: [ADR 0003](adr/0003-data-model.md).

## Stream
One compact, ordered run of data for a file: all its tokens in order, packed tightly. Like a ribbon of tokens instead of one card per token. Used in: [ADR 0003](adr/0003-data-model.md).

## Symbol
A named thing in code, such as a function, a struct or a class. It has a kind, a name and a span. Used in: [ADR 0002](adr/0002-parsing-and-crate-layout.md).

## Token
One small piece of source text, such as a word, a number or a bracket. Reading `let x = 5;` gives the tokens `let`, `x`, `=`, `5`, `;`. Used in: [ADR 0001](adr/0001-storage.md), [ADR 0002](adr/0002-parsing-and-crate-layout.md), [ADR 0003](adr/0003-data-model.md).

## Vacuum
Cleaning up a database file by rewriting it without deleted or unused data, so it gets smaller. Like tidying a cupboard. Used in: [ADR 0003](adr/0003-data-model.md).

## Write amplification
When a small write causes much more data to be written underneath. Saving one byte that makes the disk write a whole page is amplification. Used in: [learnings](learnings.md), [ADR 0003](adr/0003-data-model.md).
