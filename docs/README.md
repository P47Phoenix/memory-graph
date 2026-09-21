# Documentation index

Start here. Each document opens with a TL;DR, then details, then links to raw data.

## Suggested reading order for a newcomer
1. [Glossary](glossary.md): plain meanings of every technical word. Skim it, and come back when a word is unclear.
2. The "In plain words" box at the top of [ADR 0001](adr/0001-storage.md) and [ADR 0002](adr/0002-parsing-and-crate-layout.md). [ADR 0003](adr/0003-data-model.md) also opens with an "In plain words" section (no special vocabulary, then a table of the decisions made and the questions still open).
3. The [epic](epic-code-memory-graph.md), to see the goal and the planned stories.
4. The technical sections of the ADRs, then the [spikes](#spikes-evidence) for evidence.
5. [Learnings](learnings.md) as a quick list of key facts and pitfalls.

## Glossary
- [glossary.md](glossary.md): every technical term in plain words, in alphabetical order.

## Product
- [Epic: Language-agnostic code memory graph](epic-code-memory-graph.md): the goal, how we measure success, and the list of planned stories (1-19).

## Architecture decision records
| ADR | Status | One line |
|---|---|---|
| [0001 Storage engine](adr/0001-storage.md) | Accepted (provisional); JSON-node part proposed to be superseded by 0003 | Where the data lives: the redb database with our own graph on top, each node knowing its `parent`. |
| [0002 Parsing and crate layout](adr/0002-parsing-and-crate-layout.md) | Accepted | How the code is split into three crates, how language readers report spans, the fallback tokenizer for any language, and the pure-Rust check in CI. |
| [0003 Data model](adr/0003-data-model.md) | **Proposed** (not accepted; Q4 and Q5 decided by the user 2026-09-20) | A proposal to store tokens in a much smaller form (a dictionary, one stream per file, and count postings) instead of one record per token; also covers sharding, snapshots and migration. Opens with an "In plain words" section. Decided: an owning daemon for cross-process access, and shard by (org, repo) with the sharding build deferred. |

## Spikes (evidence)
| Spike | TL;DR | Raw data |
|---|---|---|
| [Parsing](spikes/parser.md) | Experiment on reading code: `syn` finds Rust symbols, the fallback tokenizer finds tokens. | none |
| [Storage](spikes/storage.md) | Experiment on the database: redb works, at about 690 bytes per token. | none |
| [Data model](spikes/data-model.md) | Experiment on size: the cost per token is the record around it, not the text; a stream model measured about 25x smaller. | [spikes/data-model/](../spikes/data-model/README.md) (code, README, logs) |
| [Q4/Q5 decision paper](spikes/q4-q5-decision-paper.md) | Decision record (not an ADR), decided by the user 2026-09-20: a daemon for cross-process access, shard by (org, repo) with the build deferred; keeps the options and evidence. | none |
| [Daemon and locking](spikes/daemon-and-locking.md) | Experiment on cross-process access: a socket daemon adds under 1 ms per query even at 9.9 M tokens (5 ms trigger not tripped); today's lock is held for a whole index run (20 s per 2.5 M tokens) and retries starve under load. | [spikes/daemon/](../spikes/daemon/README.md) (code, README, logs) |

## Learnings
- [learnings.md](learnings.md): a short list of lasting facts, measured numbers, rules and review mistakes to avoid, each linking to the details.

The `spikes/` directory at the repository root holds spike code that is deliberately **not** built by the cargo workspace.
