# Documentation index

Start here. Each document opens with a TL;DR, then details, then links to raw data.

## Product
- [Epic: Language-agnostic code memory graph](epic-code-memory-graph.md): goal, success metrics, story map (stories 1-19).

## Architecture decision records
| ADR | Status | One line |
|---|---|---|
| [0001 Storage engine](adr/0001-storage.md) | Accepted (provisional); JSON-node part proposed to be superseded by 0003 | redb with a custom graph layer, nodes carry `parent`. |
| [0002 Parsing and crate layout](adr/0002-parsing-and-crate-layout.md) | Accepted | core / store / cli crates, extractors return spans, fallback tokenizer, pure-Rust CI gate. |
| [0003 Data model](adr/0003-data-model.md) | **Proposed** | Dictionary + per-file streams + count postings instead of a node per token; sharding, snapshots and migration framework specified. |

## Spikes (evidence)
| Spike | TL;DR | Raw data |
|---|---|---|
| [Parsing](spikes/parser.md) | `syn` for Rust symbols, fallback tokenizer for tokens. | none |
| [Storage](spikes/storage.md) | redb works, ~690 B/token. | none |
| [Data model](spikes/data-model.md) | Token cost is the node envelope, not the text; a stream model measured ~25x smaller. | [spikes/data-model/](../spikes/data-model/README.md) (code, README, logs) |
| [Q4/Q5 decision paper](spikes/q4-q5-decision-paper.md) | Architect recommendation, awaiting user decision (not an ADR): shard granularity and cross-process access. | none |

## Learnings
- [learnings.md](learnings.md): durable facts, measured numbers, gate rules and review pitfalls, linking to the details.

The `spikes/` directory at the repository root holds spike code that is deliberately **not** built by the cargo workspace.
