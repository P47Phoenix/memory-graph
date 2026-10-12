# Measurement: gRPC overhead of `RemoteStore` versus the embedded store

Status: stage A evidence (epic story 20, [ADR 0004](../adr/0004-client-server-and-replication.md) D1 revisit trigger; [ADR 0003](../adr/0003-data-model.md) Q5 via the [daemon spike](daemon-and-locking.md)). Date 2026-09-28. Not a decision.

**TL;DR (plain language).**
- **The server adds about 0.13 to 0.16 ms per query at the median, and 0.06 ms for an empty call. The 5 ms trigger is not tripped, by a factor of about 30.** [M]
- That is the same order as the daemon spike's Unix-socket numbers (0.1 to 0.4 ms for a 100-row answer), so TCP, HTTP/2 framing and protobuf did not change the order of magnitude, as ADR 0004 D1 expected. [M]
- Measuring it found one avoidable cost, fixed in the same change: a `search` without `--limit` paid three extra round trips (open a snapshot, page, close it) even when the answer was small. The server now marks `applied_default_limit` only when its default limit (1000) filled the page, so a small answer is one round trip (0.85 ms of overhead before, 0.13 ms after). [M]
- Not measured here: a 10 M-token database (the vendored corpus is 0.24 M tokens) and a real network hop. The daemon spike showed the overhead does not grow with database size (it grows with answer size); a network adds its own round-trip time on top. [E]

**Labels.** [M] measured on this machine, [E] estimated (method stated). Machine: AMD Ryzen 9 7950X (16 cores, 32 threads), 63 GB RAM, NVMe SSD, Windows 11 Pro (10.0.26200), rustc 1.98.1, `--release`, tonic 0.14.6, redb 2.6.3. Other programs were running; warm page cache.

## Method

- Harness: `crates/graph-client/examples/rpc_bench.rs`. It opens one database file embedded (`open_store`) and times each operation, drops it, then starts a server on the same file in the same process (`graph_server::testing::TestServer`, its own tokio runtime, `127.0.0.1:0`) and times the same operations through `RemoteStore` (a real TCP connection through tonic, prost, the server's services and the store). Overhead = remote p50 minus embedded p50 on the same file.
- Each operation: 100 warm-up calls, then 1,000 timed calls; p50 and p95 of those.
- Data: the vendored corpus (`testdata/corpus`, nine repos, 660 files, 243,987 tokens, 16 MB database), indexed by the release CLI (`memory-graph --db <file> index --org corpus --repo <name> testdata/corpus/<name>` for each repo).
- Operations: `ping` (`grpc.health.v1` `Check`, the smallest round trip the client exposes), `describe` (all repos), a rare-token `search` (`Subscribe`, 19 hits, no limit), the same with `limit 1000` (what the server runs for an unlimited request), and a common-token `search` (`public`, limit 100: 100 hits).

```sh
cargo run --release -p graph-client --example rpc_bench -- corpus.redb --iters 1000 --rare Subscribe --common public
```

## Results

Milliseconds. [M]

| operation | embedded p50 | embedded p95 | remote p50 | remote p95 | overhead p50 |
|---|---:|---:|---:|---:|---:|
| ping (Health.Check) | - | - | 0.059 | 0.084 | 0.059 |
| describe | 0.053 | 0.057 | 0.188 | 0.243 | 0.135 |
| search rare (19 hits, no limit) | 0.150 | 0.166 | 0.279 | 0.338 | 0.129 |
| search rare, limit 1000 | 0.150 | 0.191 | 0.278 | 0.336 | 0.129 |
| search common, limit 100 | 0.671 | 0.810 | 0.835 | 1.019 | 0.163 |

Before the `applied_default_limit` fix, the unlimited rare search measured 0.996 ms remote p50 (1.172 p95), an overhead of 0.847 ms: the client saw `applied_default_limit` on every unlimited request and re-ran it page by page under a fresh snapshot handle. [M]

## Verdict

- ADR 0004 D1 / ADR 0003 Q5 trigger ("measured RPC overhead exceeds 5 ms at p50"): **not tripped**. The largest overhead measured is 0.16 ms at p50 for a 100-row answer. [M]
- The 10 M-token point in the story's acceptance criterion was not run here; the daemon spike measured, at 9.9 M tokens, that overhead depends on answer size, not database size, so the corpus numbers are the relevant ones for typical answers. Re-run the harness on a large database (`rpc_bench <db>`) when one is at hand. [E]

## OpenTelemetry traces on versus off (epic story 51, ADR 0009 D5)

Date 2026-10-10, the same machine as above (AMD Ryzen 9 7950X, 63 GB RAM, NVMe, Windows 11 Pro 10.0.26200), `--release`, other programs running, warm page cache. [M]

- **Method.** The same harness, run twice back to back on two copies of one corpus database (the vendored corpus indexed by the release CLI, 16 MB). One run is with OpenTelemetry off. The other uses `--otlp`: the in-process server exports traces to an in-process `FakeCollector`, and the bench's own subscriber carries the OpenTelemetry layer, so client spans and W3C propagation are on too. `--writes 200` adds single-file writes (`index_bytes`, a new file each), so the leader registers and looks up an `ApplyLinks` entry per write. Both runs use the default batch processor (5 s delay).

```sh
cargo run --release -p graph-client --example rpc_bench -- off.redb --iters 1000 --rare Subscribe --common public --writes 200
cargo run --release -p graph-client --example rpc_bench -- on.redb  --iters 1000 --rare Subscribe --common public --writes 200 --otlp
```

Remote latency in milliseconds; the embedded columns of the same runs agree within noise. [M]

| operation | off p50 | off p95 | on p50 | on p95 | on - off p50 |
|---|---:|---:|---:|---:|---:|
| ping (Health.Check) | 0.069 | 0.101 | 0.081 | 0.122 | 0.012 |
| describe | 0.472 | 0.546 | 0.504 | 0.628 | 0.032 |
| search rare (19 hits) | 0.250 | 0.318 | 0.282 | 0.379 | 0.032 |
| search rare, limit 1000 | 0.242 | 0.296 | 0.273 | 0.344 | 0.031 |
| search common, limit 100 | 1.033 | 1.207 | 1.065 | 1.211 | 0.032 |
| write (index_bytes) | 4.364 | 4.942 | 4.857 | 5.275 | 0.493 |

- The "on" run exported 11,663 spans.
- **`ApplyLinks` hashing alone**, with the same `DefaultHasher` over the entry bytes, done once at propose and once at apply on the leader: p50 **0.136 ms for a 1 MiB entry** and **0.543 ms for 4 MiB**. For the small writes above, the entry is a few hundred bytes, so hashing is negligible there. [M]
- **Verdict.**
  - Reads with traces on cost about 0.01 to 0.03 ms more at p50: the client and server spans plus the propagation headers.
  - A small write costs about 0.5 ms more at p50 (about 11%). That covers more spans per write (`client`, `rpc`, `apply`, the link), not hashing. This is a single run with the processes sharing one machine, so treat it as an upper estimate. [M]
  - With traces off, `ApplyLinks` costs one atomic load per apply and nothing at propose. [E: from the code]
  - A 4 MiB index chunk pays about 1.1 ms of hashing on the leader with traces on. That is small next to the chunk's own apply. [E: hash time times two]

## OpenTelemetry: all three signals on versus off (epic story 54)

Date 2026-10-12, the same machine as above (AMD Ryzen 9 7950X, 63 GB RAM, NVMe, Windows 11 Pro 10.0.26200), `--release`, other programs running, warm page cache. [M]

- **Method.** The story 51 harness with one addition: `--otlp-signals traces,metrics,logs` (implies `--otlp`) also gives the in-process server the OTLP metrics pipeline (`--otlp-metrics-interval` 5 s) and puts the OTLP log bridge, at INFO like `serve`'s default, in the bench's subscriber. Three configurations, each run three times, interleaved (off, traces, all; off, traces, all; ...), each on a fresh copy of the same corpus database (16 MB) with its Raft sidecars removed. The table gives the median of the three runs' p50 and p95. Total wall time: about 2 minutes.

```sh
cargo run --release -p graph-client --example rpc_bench -- x.redb --iters 1000 --rare Subscribe --common public --writes 200
cargo run --release -p graph-client --example rpc_bench -- x.redb --iters 1000 --rare Subscribe --common public --writes 200 --otlp
cargo run --release -p graph-client --example rpc_bench -- x.redb --iters 1000 --rare Subscribe --common public --writes 200 --otlp-signals traces,metrics,logs
```

Remote latency in milliseconds. [M]

| operation | off p50 | off p95 | traces p50 | traces p95 | all three p50 | all three p95 | all - off p50 |
|---|---:|---:|---:|---:|---:|---:|---:|
| ping (Health.Check) | 0.064 | 0.098 | 0.083 | 0.127 | 0.085 | 0.121 | 0.021 |
| describe | 0.466 | 0.538 | 0.477 | 0.591 | 0.477 | 0.549 | 0.011 |
| search rare (19 hits) | 0.251 | 0.335 | 0.263 | 0.331 | 0.281 | 0.352 | 0.030 |
| search rare, limit 1000 | 0.245 | 0.298 | 0.275 | 0.342 | 0.268 | 0.327 | 0.023 |
| search common, limit 100 | 0.837 | 0.952 | 0.859 | 0.966 | 0.849 | 0.953 | 0.012 |
| write (index_bytes) | 4.351 | 17.160 | 4.239 | 4.681 | 4.214 | 4.579 | -0.137 |

- Each "on" run exported 11,663 spans; the "all three" runs also exported 61 log records. Their remote phase lasts about 6 s, so metrics were exported about once (at shutdown) and the per-interval snapshot collection is not in these numbers; it runs once per interval (60 s by default) off the request path, and opens no redb transaction (story 52). [M]
- **Verdict.**
  - Reads: all three signals cost 0.01 to 0.03 ms at p50 over off, the same as traces alone. Logs add nothing measurable: `serve` logs RPCs at DEBUG, so at the INFO default a read emits no log record. [M]
  - Writes: no difference above noise this time (story 51's single run measured +0.5 ms for traces). The off runs' p95 of 17 ms comes from two of three runs with a slow tail while other programs were running, not from OpenTelemetry. [M]
  - With OpenTelemetry off nothing is built (story 50), so "off" is the cost of the code paths that check it. [E: from the code]
