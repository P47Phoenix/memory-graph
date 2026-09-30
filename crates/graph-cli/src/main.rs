// `cluster status --json` is one `json!` literal past the default limit.
#![recursion_limit = "256"]
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use graph_cli::target::{Target, TargetArgs};
use graph_cli::{index_dir, DirOpts};
use graph_client::{ReadMode, RemoteStore};
use graph_core::{Extractor, TokenClass};
use graph_store::{
    open_store, Grain, IndexOptions, Query, Store, StoreError, SymbolQuery, V2Store,
};
use std::io::Write;
use std::path::PathBuf;

/// Write a line to stdout, propagating errors (a closed pipe is handled in `main`).
macro_rules! out {
    ($($a:tt)*) => {
        writeln!(std::io::stdout().lock(), $($a)*)?
    };
}

#[derive(Parser)]
#[command(name = "memory-graph", about = "Language-agnostic code memory graph")]
struct Cli {
    /// Database file, opened in this process (default ./graph.redb). Not together with --server
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Use the `memory-graph serve` at this host:port instead of a local file; several nodes of
    /// one cluster as a comma-separated list (the first that answers is used, an unreachable or
    /// leaderless one is skipped). Also read from MEMORY_GRAPH_SERVER; the flag wins. Not
    /// together with --db. With --json, read commands add `stale_possible`
    #[arg(long, global = true, value_name = "HOST:PORT[,HOST:PORT...]")]
    server: Option<String>,
    /// With --server: how reads are served, `local` (the node's store as it is) or `linearizable`
    /// (sees every acknowledged write). Also read from MEMORY_GRAPH_READ. Default local
    #[arg(long, global = true, value_parser = graph_cli::target::parse_read_mode)]
    read: Option<ReadMode>,
    /// With --server: how long a write keeps retrying (no leader, a lost connection) before it
    /// fails with exit code 4, e.g. `500ms`, `10s`, `2m`. Also read from MEMORY_GRAPH_WRITE_DEADLINE.
    /// Default 10s
    #[arg(long, global = true, env = "MEMORY_GRAPH_WRITE_DEADLINE", value_name = "DURATION",
          value_parser = graph_cli::target::parse_deadline)]
    write_deadline: Option<std::time::Duration>,
    /// With --server: how long a read keeps retrying (no leader for a linearizable read, a node
    /// unreachable) before it fails, with exit code 4 when no leader answered, e.g. `500ms`,
    /// `10s`. It also bounds the first connect. Also read from MEMORY_GRAPH_READ_DEADLINE.
    /// Default 5s
    #[arg(long, global = true, env = "MEMORY_GRAPH_READ_DEADLINE", value_name = "DURATION",
          value_parser = graph_cli::target::parse_deadline)]
    read_deadline: Option<std::time::Duration>,
    /// Deprecated, hidden: there is one storage format now. `--backend v2` is accepted as a no-op for old
    /// scripts; `--backend v1` is an error that says where the retired format went
    #[arg(long, global = true, hide = true, value_enum)]
    backend: Option<LegacyBackend>,
    /// Commit a write transaction every this many source bytes ingested, continuing the batch in a new one
    /// (default 64 MiB). A soft cap: one file larger than it is still a chunk of its own
    #[arg(long, global = true, alias = "v2-chunk-bytes", value_parser = clap::value_parser!(u64).range(1..))]
    chunk_bytes: Option<u64>,
    /// Cache size in bytes for the database (redb's default is 1 GiB, split 9:1 between its read and write
    /// caches)
    #[arg(long, global = true, alias = "v2-cache-bytes", value_parser = clap::value_parser!(u64).range(1..))]
    cache_bytes: Option<u64>,
    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    fn overrides(&self) -> Overrides {
        Overrides {
            chunk_bytes: self.chunk_bytes,
            cache_bytes: self.cache_bytes,
        }
    }

    /// Where the store is, from the flags and the environment.
    fn target(&self) -> Result<Target> {
        let (server_env, read_env) = TargetArgs::env();
        graph_cli::target::resolve(TargetArgs {
            db: self.db.as_deref(),
            server_flag: self.server.as_deref(),
            server_env: server_env.as_deref(),
            read_flag: self.read,
            read_env: read_env.as_deref(),
        })
    }
}

/// Settings that only make sense for a local file are refused with a
/// server, saying where they belong instead.
fn refuse_embedded_only_flags(o: Overrides) -> Result<()> {
    if o.chunk_bytes.is_some() {
        bail!(
            "--chunk-bytes does not apply with --server: the server cuts replicated log entries at {} MiB itself",
            graph_client::RAFT_ENTRY_MAX_BYTES >> 20
        );
    }
    if o.cache_bytes.is_some() {
        bail!("--cache-bytes does not apply with --server: pass it to `memory-graph serve`");
    }
    Ok(())
}

/// The store a command reads or writes: the local file (with `extractors`
/// and `overrides`, retrying while it is locked), or a server connection.
fn open_target(
    target: &Target,
    extractors: fn() -> Vec<Box<dyn Extractor>>,
    overrides: Overrides,
) -> Result<Box<dyn Store>> {
    match target {
        Target::Embedded(db) => graph_cli::target::open_embedded(db, || {
            open_with_overrides(db, extractors(), overrides)
        }),
        Target::Remote { addr, read } => {
            refuse_embedded_only_flags(overrides)?;
            Ok(Box::new(graph_cli::target::connect(addr, *read)?))
        }
    }
}

/// A server connection for a command that needs the concrete client
/// (admin calls), with the embedded-only flags refused.
fn remote(addr: &str, read: ReadMode, overrides: Overrides) -> Result<RemoteStore> {
    refuse_embedded_only_flags(overrides)?;
    graph_cli::target::connect(addr, read)
}

/// Values the retired `--backend` flag still parses. Kept so `--backend v2`
/// in an existing script keeps working and `--backend v1` fails with a
/// pointer to the migration path instead of an unknown-flag error.
/// `serve --mcp-read`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum McpReadArg {
    Local,
    Linearizable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum LegacyBackend {
    V1,
    V2,
}

/// The retired v1 format cannot be selected: say where it went.
fn reject_legacy_backend(backend: Option<LegacyBackend>) -> Result<()> {
    if backend == Some(LegacyBackend::V1) {
        bail!(
            "`--backend v1` is not supported: the v1 format was retired; this release reads only the v2 \
             format. Re-index from source into a new file (recommended), or convert an existing v1 database \
             with the last v1-capable release (git tag `v1-last`): `memory-graph migrate <new.redb>`"
        );
    }
    Ok(())
}

#[derive(Subcommand)]
enum ClusterCmd {
    /// The node's Raft view: role, term, leader, applied/committed log index, versions
    Status {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print the current leader's id and address; exit code 3 when no leader is known
    Leader {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Build a Raft snapshot on the server now; with --out, download it (sha256 and size verified)
    /// to this local file, which `serve --data-dir <empty dir> --bootstrap --restore <file>` can
    /// seed a new cluster from
    Snapshot {
        /// Where to write the snapshot (on this machine, not the server's)
        #[arg(long, value_name = "FILE", conflicts_with = "upload")]
        out: Option<PathBuf>,
        /// Have the leader (any node forwards it) build a snapshot now and upload it to its
        /// backup location (`serve --backup-url`, whatever --backup-on says); prints the backup's
        /// URL, which `serve --bootstrap --restore` takes, and its sha256
        #[arg(long)]
        upload: bool,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// The committed backups (with their .meta) of this cluster in the connected node's backup
    /// location (`serve --backup-url`), newest first: URL, term, index, size, sha256
    Backups {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Every member: id, role (voter or learner), address, and which one leads
    Members {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Add node <ID>, already serving at <ADDR> (host:port), as a non-voting learner (a node
    /// started with `serve --join` asks for this itself). The node is asked who it is first:
    /// another node id, another cluster (exit code 6) or other extractors are refused. Waits
    /// until it caught up with the leader's log
    AddLearner {
        /// The new node's id (its --node-id)
        id: u64,
        /// The host:port the new node serves at (its --advertise address)
        addr: String,
        /// Return once the change is committed, without waiting for the catch-up
        #[arg(long)]
        no_wait: bool,
    },
    /// Make learner <ID> a voter (joint consensus). Refused for an unknown node, a voter, or a
    /// node whose extractor version set differs from the cluster's
    Promote {
        /// The learner's node id
        id: u64,
    },
    /// Remove node <ID> from the cluster. Refused for the leader (transfer leadership first),
    /// for anything that would leave fewer reachable voters than a quorum, and for 3 voters down
    /// to 2 unless --force
    Remove {
        /// The node id to remove
        id: u64,
        /// Allow going from 3 voters to 2 (a cluster that then tolerates no failure). Never
        /// overrides the other guards
        #[arg(long)]
        force: bool,
    },
    /// Make voter <ID> the leader: the leader waits until <ID> caught up, pauses its heartbeats
    /// and writes (clients retry) until its lease runs out, and asks <ID> to call an election
    TransferLeader {
        /// The voter's node id
        id: u64,
    },
}

// Parsed once per process; `Serve`'s many flags make it the large variant.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Serve a database over gRPC; other processes, machines and containers then use it with
    /// --server. `--data-dir <dir> --bootstrap` starts a one-node cluster in a data directory,
    /// `--data-dir <dir> --join <peer>` joins an existing one; `--db <file>` serves a single file.
    /// Prints `listening on <addr>` once ready (after joining, with --join); stops on Ctrl-C /
    /// SIGTERM. Exit code 6: the data directory belongs to another cluster than --join's peer
    #[command(group(
        clap::ArgGroup::new("backup_or_restore")
            .args(["backup_url", "restore"])
            .multiple(true)
    ))]
    Serve {
        /// Read these settings from a TOML file: every serve flag is a key of the same name
        /// (kebab-case or snake_case: `data-dir = "/data"`, `bootstrap = true`, `peers =
        /// ["a:7000"]`), plus `db` and `cache-bytes`. A flag on the command line (or its
        /// environment variable) overrides the file; an unknown key is an error. Env:
        /// MEMORY_GRAPH_CONFIG
        #[arg(long, env = "MEMORY_GRAPH_CONFIG", value_name = "FILE")]
        config: Option<PathBuf>,
        /// Address to listen on (port 0 picks a free port; the line printed on start names it)
        #[arg(long, default_value = "127.0.0.1:7000", value_name = "HOST:PORT")]
        listen: String,
        /// The node's data directory (node.json, graph.redb, raft.redb, snapshots/, LOCK).
        /// Not together with --db
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// With --data-dir: create a NEW cluster with this node as its only voter (on a directory
        /// that is already initialized: a plain restart)
        #[arg(long, requires = "data_dir")]
        bootstrap: bool,
        /// With --bootstrap, into an empty --data-dir: seed the store from this snapshot file
        /// (`cluster snapshot --out`), or from a backup, `file://<dir>/<cluster_id>/snap-T-I.redb`,
        /// `s3://bucket/prefix/<cluster_id>/snap-T-I.redb` (with --backup-endpoint and the other
        /// --backup-* S3 settings), or `.../<cluster_id>/latest` (the highest committed index that
        /// verifies), each verified against its .meta: size, sha256, store format and extractors.
        /// A plain file's sibling .meta is verified when present. The new cluster gets a new id
        /// and a fresh log
        #[arg(long, requires = "bootstrap", value_name = "FILE|URL")]
        restore: Option<PathBuf>,
        /// With --restore: accept a backup made with other extractors (the store stays valid;
        /// affected files re-extract on their next index). The store format check has no override
        #[arg(long, requires = "restore")]
        restore_allow_extractor_mismatch: bool,
        /// Copy each snapshot this server builds to a backup location, `file://<dir>` or
        /// `s3://bucket/prefix` (with --backup-endpoint), as <dir>/<cluster_id>/snap-T-I.redb plus
        /// .meta (written last: a backup without its .meta does not count). Uploads run in the
        /// background and never delay snapshots or purges
        #[arg(long, value_name = "URL")]
        backup_url: Option<String>,
        /// With --backup-url: keep this many backups of this cluster (0: keep all); older ones
        /// are deleted .meta first, and data without a .meta older than 24 h is swept
        #[arg(long, requires = "backup_url", value_name = "N", default_value_t = graph_server::backup::DEFAULT_KEEP)]
        backup_keep: usize,
        /// With --backup-url: which nodes upload the snapshots they build: leader, all or none
        #[arg(
            long,
            requires = "backup_url",
            value_name = "WHO",
            default_value = "leader"
        )]
        backup_on: graph_server::backup::BackupOn,
        /// With an s3:// --backup-url or --restore: the S3-compatible endpoint, http://host:port
        /// (MinIO, Ceph RGW, R2, B2, Garage, or a TLS sidecar in front of AWS). Plain HTTP only:
        /// https:// is refused (no pure-Rust TLS yet, #104)
        #[arg(long, requires = "backup_or_restore", value_name = "URL")]
        backup_endpoint: Option<String>,
        /// With an s3:// --backup-url or --restore: the region requests are signed for
        #[arg(long, requires = "backup_or_restore", value_name = "REGION", default_value = graph_server::backup::s3::DEFAULT_REGION)]
        backup_region: String,
        /// With an s3:// --backup-url or --restore: virtual-hosted addressing
        /// (http://bucket.host/key) instead of path-style (http://host/bucket/key)
        #[arg(long, requires = "backup_or_restore")]
        backup_virtual_host: bool,
        /// With an s3:// --backup-url or --restore: an AWS credentials file (INI) to read the keys
        /// from when AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN) are not
        /// set, which win. Keys are never taken from flags or the config file
        #[arg(long, requires = "backup_or_restore", value_name = "FILE")]
        backup_credentials_file: Option<PathBuf>,
        /// With --backup-credentials-file: the profile to read (default: default)
        #[arg(long, requires = "backup_credentials_file", value_name = "NAME")]
        backup_profile: Option<String>,
        /// With --data-dir: join the cluster that the node at this host:port belongs to (any
        /// member; it forwards to the leader). On an empty directory the node asks to be added as
        /// a learner and catches up; on one that already belongs to that cluster it is a plain
        /// restart; one of another cluster is refused (exit code 6)
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "data_dir",
            conflicts_with = "bootstrap"
        )]
        join: Option<String>,
        /// For a StatefulSet: ordinal 0 (host name ending in `-0`) bootstraps, the others --join
        /// <HOST:PORT> (ordinal 0's address). Ordinal 0 on an empty data directory first asks the
        /// other members (--peers) whether a cluster exists and joins it instead (a lost volume
        /// never creates a second cluster). Restarts are plain restarts either way
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "data_dir",
            conflicts_with_all = ["bootstrap", "join", "restore"]
        )]
        bootstrap_or_join: Option<String>,
        /// With --bootstrap-or-join: the other members ordinal 0 asks for an existing cluster,
        /// comma-separated HOST:PORTs (default: ordinals 1..--replicas of the named peer's
        /// StatefulSet, e.g. memory-graph-1.<same domain>:<same port>)
        #[arg(
            long,
            requires = "bootstrap_or_join",
            value_delimiter = ',',
            value_name = "HOST:PORT,..."
        )]
        peers: Vec<String>,
        /// With --bootstrap-or-join and no --peers: the StatefulSet's replica count
        #[arg(
            long,
            requires = "bootstrap_or_join",
            default_value_t = 3,
            value_name = "N"
        )]
        replicas: u64,
        /// With --bootstrap-or-join: how long ordinal 0 asks the other members before it
        /// bootstraps (it stops early once all of them answered they have no cluster)
        #[arg(
            long,
            requires = "bootstrap_or_join",
            value_parser = parse_duration,
            default_value = "30s",
            value_name = "DURATION"
        )]
        bootstrap_probe_timeout: std::time::Duration,
        /// With --bootstrap-or-join: ordinal 0 bootstraps an empty data directory without asking
        /// the other members (a deliberate new cluster)
        #[arg(long, requires = "bootstrap_or_join")]
        force_bootstrap: bool,
        /// With --join (or --bootstrap-or-join): become a voter once caught up (the leader
        /// promotes the node when its replication lag is zero)
        #[arg(long, conflicts_with = "standby")]
        auto_promote: bool,
        /// With --join: stay a learner (a read replica) until `cluster promote`; the default
        /// without --auto-promote, said explicitly
        #[arg(long, requires = "join")]
        standby: bool,
        /// With --join: a directory that holds a store (or a Raft log) but no node.json is joined
        /// anyway; what it holds is moved aside into replaced-<time>/ and the leader's data
        /// replaces it
        #[arg(long, requires = "join")]
        accept_snapshot_overwrite: bool,
        /// With --join: how long the first join keeps retrying while no leader answers, e.g.
        /// `30s`, `2m` (default 2m)
        #[arg(long, requires = "join", value_parser = parse_duration, default_value = "2m")]
        join_timeout: std::time::Duration,
        /// host:port peers and clients reach this node at (default: the listen address with a
        /// wildcard IP replaced by the host name). Stored in node.json
        #[arg(long, value_name = "HOST:PORT")]
        advertise: Option<String>,
        /// With --data-dir, on a restart: this node now listens at HOST:PORT (a new IP, port or
        /// DNS name). Once serving, it asks the leader (through any member it knows) to record
        /// the new address in the membership, then rewrites node.json; the start fails, and
        /// node.json stays as it was, if no leader accepts it within 2 minutes. The same
        /// address again is a plain restart. (A different --advertise is refused instead)
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "data_dir",
            conflicts_with = "advertise"
        )]
        update_advertise: Option<String>,
        /// This node's id. --db: default 1. --data-dir: required on the first start, then read
        /// from node.json (a different id is refused)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        node_id: Option<u64>,
        /// Take the node id from the host name's trailing `-<ordinal>` plus one (a StatefulSet
        /// pod `memory-graph-2` is node 3), instead of --node-id
        #[arg(long, requires = "data_dir", conflicts_with = "node_id")]
        node_id_from_hostname: bool,
        /// Log format on stderr: text, or json (one object per line: timestamp, level, target,
        /// message, span)
        #[arg(long, value_enum, default_value = "text")]
        log_format: graph_cli::logging::LogFormat,
        /// Log filter, tracing EnvFilter syntax: `info`, `debug`, `info,graph_server=debug`
        /// (default info). Env: MEMORY_GRAPH_LOG
        #[arg(
            long,
            env = "MEMORY_GRAPH_LOG",
            default_value = "info",
            value_name = "FILTER"
        )]
        log_level: String,
        /// Serve Prometheus metrics (text format 0.0.4) at http://<HOST:PORT>/metrics
        #[arg(long, value_name = "HOST:PORT")]
        metrics_listen: Option<String>,
        /// Serve MCP (Model Context Protocol, streamable HTTP; read-only tools) at
        /// http://<HOST:PORT>/mcp, on a port of its own. Off unless given. Loopback only
        /// (127.0.0.1, ::1) unless --mcp-allow-remote. See docs/mcp.md.
        /// +----------------------------------------------------------------------+
        /// | WARNING: The MCP endpoint has no authentication. Anyone who can      |
        /// | reach it can read every indexed source token. Keep it on loopback    |
        /// | or behind an authenticating proxy until #105.                        |
        /// +----------------------------------------------------------------------+
        #[arg(long, value_name = "HOST:PORT", verbatim_doc_comment)]
        mcp_listen: Option<String>,
        /// Let --mcp-listen be a non-loopback address (logs a warning at every start)
        #[arg(long, requires = "mcp_listen")]
        mcp_allow_remote: bool,
        /// A browser Origin the MCP endpoint accepts, exactly (`http://localhost:6274`);
        /// repeatable. A request with any other Origin gets 403 (default: none; requests
        /// without an Origin, such as non-browser clients, are accepted)
        #[arg(long, value_name = "URL", requires = "mcp_listen")]
        mcp_allow_origin: Vec<String>,
        /// How MCP tools read: local (this node's replica; answers say stale_possible when it
        /// may lag) or linearizable (every read after the leader's read barrier). Default local
        #[arg(long, value_enum, requires = "mcp_listen")]
        mcp_read: Option<McpReadArg>,
        /// MCP requests served at once; more get 429 (default 16)
        #[arg(
            long,
            value_name = "N",
            requires = "mcp_listen",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        mcp_max_inflight: Option<u64>,
        /// The memory-graph.ready health service is SERVING only while a leader is known and
        /// this node's applied index is within this many entries of the leader's commit index
        #[arg(long, value_name = "N", default_value_t = graph_server::DEFAULT_READY_MAX_LAG)]
        ready_max_lag: u64,
        /// Build a snapshot (and purge the log below it) after this many applied entries
        /// (default 10000)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        snapshot_log_entries: Option<u64>,
        /// ... or after this many log bytes: a size with a K/M/G suffix (default 1G)
        #[arg(long, value_name = "SIZE", value_parser = graph_cli::sysinfo::parse_size)]
        snapshot_log_bytes: Option<u64>,
        /// Log entries kept below a snapshot, so a briefly lagging follower catches up from the
        /// log rather than by a snapshot (default 1000)
        #[arg(long, value_name = "N")]
        log_keep_entries: Option<u64>,
        /// Election timeout lower bound, ms (--data-dir default 1000)
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(1..))]
        election_timeout_min: Option<u64>,
        /// Election timeout upper bound, ms (--data-dir default 2000)
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(1..))]
        election_timeout_max: Option<u64>,
        /// Leader heartbeat interval, ms (--data-dir default 250)
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(1..))]
        heartbeat_interval: Option<u64>,
        /// A leader that has heard from no quorum for this long answers its pending writes
        /// NoLeader (the client retries, then exits 4), ms (default: the election timeout max).
        /// Only quorum loss triggers it; a slow but healthy write is never cut off. A client's
        /// write to a minority leader fails after at most about its write deadline plus 1.5x
        /// this window (the window, then a liveness probe of up to half of it). NoLeader does
        /// not mean the write was not applied; retries are idempotent
        #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(1..))]
        quorum_loss_timeout: Option<u64>,
        /// Refuse writes and snapshot builds (RESOURCE_EXHAUSTED) while the volume has less than
        /// this free plus one snapshot copy: a size (K/M/G) or a percentage of the volume such as
        /// 5%. --data-dir default: 5% of the volume, clamped to 2-32 GiB. --db default: off
        #[arg(long, value_parser = graph_cli::diskinfo::parse_min_free)]
        min_free_disk: Option<graph_cli::diskinfo::MinFree>,
        /// How long an open snapshot handle (a paging client's frozen view) may live: seconds, or with
        /// an s/m/h suffix (default 15m)
        #[arg(long, value_parser = parse_duration, default_value = "15m")]
        snapshot_max_age: std::time::Duration,
    },
    /// Check a server's health (grpc.health.v1): exit 0 when serving, 1 when not (or unreachable).
    /// With --ready: the memory-graph.ready service, SERVING while a leader is known and this node
    /// has applied to within the server's --ready-max-lag of the leader's commit index (a
    /// Kubernetes readiness probe / Compose healthcheck). Exit 1 in every other case. Needs
    /// --server (or MEMORY_GRAPH_SERVER)
    Health {
        /// Check the memory-graph.ready service (a leader is known and this node is caught up)
        /// instead of the default one (the store is open)
        #[arg(long)]
        ready: bool,
    },
    /// Cluster information and membership (needs --server; any node: membership changes are
    /// forwarded to the leader)
    Cluster {
        #[command(subcommand)]
        cmd: ClusterCmd,
    },
    /// Index one file under an org and repo (re-indexing replaces it)
    IndexFile {
        /// Organization name, free-form: the top level of the graph (org -> repo -> file)
        #[arg(long)]
        org: String,
        /// Repository name, free-form: the second level of the graph, under --org
        #[arg(long)]
        repo: String,
        /// Language override (default: detected from extension)
        #[arg(long)]
        language: Option<String>,
        /// Re-index even when the file is unchanged since it was last indexed
        #[arg(long)]
        reindex: bool,
        /// The file to index; stored under the repo by this path as given
        path: PathBuf,
    },
    /// Index a directory as a repo (honors .gitignore; skips binary files)
    Index {
        /// Organization name, free-form: the top level of the graph (org -> repo -> file)
        #[arg(long)]
        org: String,
        /// Repository name, free-form: the second level of the graph, under --org
        #[arg(long)]
        repo: String,
        /// Print the summary as JSON instead of text
        #[arg(long)]
        json: bool,
        /// Skip files larger than this many bytes (lockfiles, minified bundles, dumps). Off by default:
        /// the only built-in limit is the store's 4 GiB span limit. A file is held in memory whole while
        /// it is parsed and its parse takes about 25x its size (a 1 GB file about 25 GB); the memory
        /// budget admits such a file alone
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        max_file_size: Option<u64>,
        /// Remove files of this repo that were not indexed in this run (deleted, renamed, newly ignored or
        /// skipped). Only files last indexed by a directory run are considered: `index-file` clears that
        /// mark, and a later directory run sets it again. Skipped when some paths were unreadable; refused
        /// when nothing was indexed and files would be removed, unless --force
        #[arg(long)]
        prune: bool,
        /// With --prune: allow removing files even when this run indexed nothing
        #[arg(long, requires = "prune")]
        force: bool,
        /// Re-index every file even when unchanged since it was last indexed (by default files with the
        /// same content, language and extractor version are skipped). Does not affect --prune's safety checks
        #[arg(long)]
        reindex: bool,
        /// Parse threads (default, or 0: one per CPU but one, left for the database writer). Files are still
        /// committed in walk order by one writer, so the stored content is the same for any value
        #[arg(long, short = 'j', default_value_t = 0, hide_default_value = true)]
        jobs: usize,
        /// Cap on source bytes read but not yet committed: a fixed size (2G), or the share of the free
        /// memory this process may grow into (70%, the default), divided by the measured growth per source
        /// byte, re-sampled during the run and lowered under memory pressure; at least 20% of RAM is always
        /// left for the OS. Also read from MEMORY_GRAPH_MEMORY
        #[arg(long, env = "MEMORY_GRAPH_MEMORY", value_parser = graph_cli::sysinfo::parse_memory_spec)]
        memory: Option<graph_cli::sysinfo::MemorySpec>,
        /// Commit fixed batches (256 files / 32 MiB; a file that does not fit is a batch of its own) instead
        /// of everything ready, so the database file is byte-for-byte reproducible on any machine (slower
        /// when the writer is the bottleneck). A fixed --memory must be at least one batch (32M)
        #[arg(long)]
        deterministic: bool,
        /// Print how busy each stage (walk, parse, commit) was, and the bottleneck, on stderr at the end
        /// (with --json: a `stats` object in the summary)
        #[arg(long)]
        stats: bool,
        /// Write a Chrome/Perfetto trace of the run (one span per file per stage) to this file
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Show live progress on stderr even with --json (default: only when --json is off). Never drawn
        /// when stderr is not a terminal
        #[arg(long, conflicts_with = "no_progress")]
        progress: bool,
        /// Never show live progress
        #[arg(long)]
        no_progress: bool,
        /// Free space to keep on the database's volume, e.g. 4G or 5% (default: 5%, between 2G and 32G).
        /// The run refuses to start below it and stops cleanly, resumably, if it would go below it
        #[arg(long, value_parser = graph_cli::diskinfo::parse_min_free)]
        min_free_disk: Option<graph_cli::diskinfo::MinFree>,
        /// Report disk space but never stop for it
        #[arg(long)]
        no_disk_check: bool,
        /// The directory to index as one repo; files are stored by their path relative to it
        dir: PathBuf,
    },
    /// Show what `index` sizes itself from on this machine: CPUs, memory (and where the reading came
    /// from, or why there is none), the budget it would start with, and the free space on --db's volume
    Sysinfo {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
        /// The `--memory` setting to size the budget with (default: 70%). Also read from MEMORY_GRAPH_MEMORY
        #[arg(long, env = "MEMORY_GRAPH_MEMORY", value_parser = graph_cli::sysinfo::parse_memory_spec)]
        memory: Option<graph_cli::sysinfo::MemorySpec>,
        /// The `--min-free-disk` setting to report the reserve for
        #[arg(long, value_parser = graph_cli::diskinfo::parse_min_free)]
        min_free_disk: Option<graph_cli::diskinfo::MinFree>,
    },
    /// Drop dictionary terms that no file refers to any more
    Vacuum {
        /// Also rebuild the file to reclaim the space `vacuum` frees but redb does not return on its own
        /// (single-process rebuild-then-rename)
        #[arg(long)]
        compact: bool,
    },
    /// Show what is indexed: per repo, the languages present and each language's symbol kinds
    Describe {
        /// Only repos of this org
        #[arg(long)]
        org: Option<String>,
        /// Only repos with this name
        #[arg(long)]
        repo: Option<String>,
        /// Print JSON (`{"repos": [...]}`) instead of text
        #[arg(long)]
        json: bool,
    },
    /// Find symbols (definitions) by name; a trailing `*` matches a prefix
    Symbols {
        /// Exact name, `prefix*` (a prefix), `*` (everything) or `name\*` (a name ending in a literal `*`).
        /// Edges: `**` at the end is rejected as ambiguous (so `a\**` is too), and a trailing backslash not
        /// followed by `*` is an ordinary character (`a\` matches the name `a\` exactly)
        pattern: String,
        /// Only symbols of this symbol kind (case-insensitive): generic (module, type, function, method,
        /// variable, constant, other) or language-specific (struct, trait, impl, ...). `describe` lists the
        /// kinds present. (`search --kind` is a token class, a different thing)
        #[arg(long)]
        kind: Option<String>,
        /// Only files of this language (case-insensitive, e.g. rust); `describe` lists the languages present
        #[arg(long)]
        language: Option<String>,
        /// Only this organisation (exact name)
        #[arg(long)]
        org: Option<String>,
        /// Only this repo (exact name)
        #[arg(long)]
        repo: Option<String>,
        /// Restrict to one file path as indexed (relative to the indexed directory)
        #[arg(long)]
        file: Option<String>,
        /// Show at most this many results (ordered by org, repo, file, position)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        /// Skip this many results (same order as --limit) before collecting --limit of them;
        /// paired with --limit to page through a large result set (ADR 0003 story 11)
        #[arg(long)]
        offset: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Dump the database's full node graph as newline-delimited JSON, one
    /// JSON object per node (org, repo, file, symbol, token, each with its
    /// span): a portable escape hatch, not an importer -- there is no
    /// `import` command yet
    Export {
        /// Write to this file instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Find tokens by exact text
    Search {
        /// Exact token text
        text: String,
        /// Only files of this language (case-insensitive); `describe` lists the languages present
        #[arg(long)]
        language: Option<String>,
        /// Only this organisation (exact name)
        #[arg(long)]
        org: Option<String>,
        /// Only this repo (exact name)
        #[arg(long)]
        repo: Option<String>,
        /// Token class (NOT a symbol kind; see --symbol-kind): identifier, keyword, literal, operator, punctuation, comment, other
        #[arg(long)]
        kind: Option<TokenClass>,
        /// Level results are rolled up to: token, symbol (nearest enclosing symbol), method
        /// (nearest enclosing method or function), class (nearest enclosing type, or a Rust impl
        /// block), file, repo or org. Symbol, method and class rows carry that symbol's full span
        #[arg(long, default_value = "token")]
        grain: Grain,
        /// With --grain symbol, method or class: only symbols of this symbol kind
        /// (case-insensitive; generic or language-specific; see `describe`)
        #[arg(long)]
        symbol_kind: Option<String>,
        /// Show at most this many rows (ordered by org, repo, file, position)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,
        /// Skip this many rows (same order as --limit) before collecting --limit of them;
        /// paired with --limit to page through a large result set (ADR 0003 story 11)
        #[arg(long)]
        offset: Option<u64>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Serve the Model Context Protocol over stdio, read-only (ADR 0005): an AI assistant starts
    /// this command and calls its tools (describe, list_repos, search, find_symbols, file_outline,
    /// file_tokens, list_files). Reads --db, or --server / MEMORY_GRAPH_SERVER with --read. Stdout
    /// carries only protocol messages; logs go to stderr. Client configuration: docs/mcp.md
    ///
    /// Over stdio no port is opened: only the client that started the process can talk to it.
    /// The HTTP endpoint (`serve --mcp-listen`) is another matter:
    /// +----------------------------------------------------------------------+
    /// | WARNING: The MCP endpoint has no authentication. Anyone who can      |
    /// | reach it can read every indexed source token. Keep it on loopback    |
    /// | or behind an authenticating proxy until #105.                        |
    /// +----------------------------------------------------------------------+
    #[command(verbatim_doc_comment)]
    Mcp,
}

/// Store settings taken from CLI flags. Both default to redb/`V2Store`
/// defaults (`None`).
#[derive(Clone, Copy, Default)]
struct Overrides {
    chunk_bytes: Option<u64>,
    cache_bytes: Option<u64>,
}

/// Open the store at `db` with `extractors` registered, applying
/// `overrides`. Goes through `open_store` whenever no override applies, so it
/// only diverges from `open_store`'s construction path (`V2Store::open` +
/// register) when an override needs a `V2Store` method
/// (`open_with_cache_bytes`, `set_chunk_bytes`) that isn't on the `Store`
/// trait; mirror any future change to `open_store` here too. A file in the
/// retired v1 format is refused (`StoreError::LegacyFormat`) before anything
/// is written, with the migration hint in the error.
fn open_with_overrides(
    db: &std::path::Path,
    extractors: Vec<Box<dyn Extractor>>,
    overrides: Overrides,
) -> std::result::Result<Box<dyn Store>, StoreError> {
    if overrides.chunk_bytes.is_some() || overrides.cache_bytes.is_some() {
        let mut s = open_v2(db, overrides)?;
        for e in extractors {
            s.register(e);
        }
        return Ok(Box::new(s));
    }
    open_store(db, extractors)
}

/// A concrete `V2Store` with `overrides` applied (for `vacuum --compact`,
/// which needs an owned store).
fn open_v2(db: &std::path::Path, overrides: Overrides) -> std::result::Result<V2Store, StoreError> {
    let mut s = V2Store::open_with_cache_bytes(db, overrides.cache_bytes.map(|b| b as usize))?;
    if let Some(bytes) = overrides.chunk_bytes {
        s.set_chunk_bytes(bytes as usize);
    }
    Ok(s)
}

/// Open the store for a command that indexes, with every shipped extractor
/// registered: the extractor version is part of a file's fingerprint, so
/// indexing without one would downgrade already-indexed files to tokens only.
/// (A server registers its own extractors; see `serve`.)
fn open_for_indexing(target: &Target, overrides: Overrides) -> Result<Box<dyn Store>> {
    open_target(target, graph_cli::shipped_extractors, overrides)
}

/// Open the store for a query: a local file must already exist.
fn open_existing(target: &Target, overrides: Overrides) -> Result<Box<dyn Store>> {
    if let Target::Embedded(db) = target {
        if db.is_dir() {
            bail!(
                "database `{}` is a directory, not a database file",
                db.display()
            );
        }
        if !db.is_file() {
            bail!("database `{}` does not exist", db.display());
        }
        return open_target(target, Vec::new, overrides)
            .with_context(|| format!("opening database `{}`", db.display()));
    }
    open_target(target, Vec::new, overrides)
}

/// Filters are checked against what is actually indexed (in the org/repo
/// scope), so a typo fails loudly and lists the valid choices rather than
/// returning nothing.
fn validate_filters(
    store: &dyn Store,
    org: Option<&str>,
    repo: Option<&str>,
    language: Option<&str>,
    kind: Option<&str>,
) -> Result<()> {
    for (name, v) in [
        ("--org", org),
        ("--repo", repo),
        ("--language", language),
        ("--kind/--symbol-kind", kind),
    ] {
        if v == Some("") {
            bail!("{name} must not be empty");
        }
    }
    let infos = store.describe(org, repo)?;
    if infos.is_empty() {
        if org.is_some() || repo.is_some() {
            bail!("no indexed repo matches --org/--repo; run `describe` to list what is indexed");
        }
        return Ok(());
    }
    if let Some(l) = language {
        let langs: std::collections::BTreeSet<&String> =
            infos.iter().flat_map(|i| i.languages.keys()).collect();
        if !langs.iter().any(|x| x.eq_ignore_ascii_case(l)) {
            bail!(
                "no files of language `{l}` in scope; languages present: {}",
                join(langs.iter().copied())
            );
        }
    }
    if let Some(k) = kind {
        let kinds: std::collections::BTreeSet<String> =
            infos.iter().flat_map(|i| i.kind_names(language)).collect();
        // Generic kinds are always valid (`other` covers symbols without a kind).
        let known = kinds.iter().any(|x| x.eq_ignore_ascii_case(k));
        if !known
            && k.to_ascii_lowercase()
                .parse::<graph_core::SymbolKind>()
                .is_err()
        {
            bail!(
                "no symbols of kind `{k}` in scope; kinds present: {}",
                join(kinds.iter())
            );
        }
    }
    Ok(())
}

fn join<'a>(it: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = it.map(String::as_str).collect();
    if v.is_empty() {
        "(none)".into()
    } else {
        v.join(", ")
    }
}

/// `--snapshot-max-age`: whole seconds, or a number with an s/m/h suffix.
fn parse_duration(s: &str) -> std::result::Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a duration (e.g. 900, 900s, 15m, 1h)"))?;
    if n == 0 {
        return Err("the duration must be positive".into());
    }
    Ok(std::time::Duration::from_secs(n.saturating_mul(mult)))
}

/// Set once `serve --log-format json` installed its subscriber: a failure
/// is then logged as JSON rather than printed as text.
static JSON_LOGS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// The stack `run` gets, on a thread of its own. Needed for the debug-build
/// frame size of `run()` (one large `match` over every command, unoptimized):
/// it outgrew Windows' 1 MiB main-thread stack when `serve` gained
/// `--quorum-loss-timeout`, and every CLI test failed with a stack overflow.
/// A release build runs within 1 MiB (checked on Windows, 2026-09-29, with
/// `run` on a 1 MiB thread: `describe` and `index` work); 8 MiB costs only
/// address space, so both builds use it.
const MAIN_STACK_BYTES: usize = 8 << 20;

fn main() {
    let result = std::thread::Builder::new()
        .name("main".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(run)
        .expect("spawning the main thread")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // `symbols | head` closes the pipe early: that is not an error.
            let broken = e
                .chain()
                .filter_map(|c| c.downcast_ref::<std::io::Error>())
                .any(|io| io.kind() == std::io::ErrorKind::BrokenPipe);
            if broken {
                std::process::exit(0);
            }
            let code = graph_cli::target::exit_code(&e);
            let e = match REMOTE_ADDR.get() {
                Some(addr) => graph_cli::target::explain_remote_failure(
                    e,
                    addr,
                    code,
                    REMOTE_IS_READ.get().copied().unwrap_or(false),
                ),
                None => e,
            };
            if JSON_LOGS.get().copied().unwrap_or(false) {
                // `serve --log-format json`: stderr stays one JSON object
                // per line, the failure included.
                tracing::error!(error = %format!("{e:#}"), exit_code = code, "memory-graph serve failed");
            } else {
                eprintln!("Error: {e:?}");
            }
            std::process::exit(code);
        }
    }
}

/// `sysinfo`'s report as the server hands it out (`Admin.SysInfo`): the
/// JSON document plus its text rendering under `text`, so `sysinfo
/// --server` prints the same text a local `sysinfo` would.
fn server_sysinfo(db: &std::path::Path) -> serde_json::Value {
    let r = graph_cli::report::Report::detect(db, None, graph_cli::diskinfo::MinFree::Default);
    let mut j = r.json();
    j["text"] = serde_json::Value::String(r.text());
    j
}

/// `server <addr> node N (leader: M)`.
/// How an MCP bound address is shown: `off` when empty, the URL for a
/// specific address, and no URL for a wildcard bind (0.0.0.0 / [::]),
/// which is not an address a client can use.
fn mcp_url(addr: &str) -> String {
    match addr.parse::<std::net::SocketAddr>() {
        Err(_) if addr.is_empty() => "off".into(),
        Ok(a) if a.ip().is_unspecified() => format!(
            "bound on all interfaces, port {} (path /mcp; use this host's address)",
            a.port()
        ),
        _ => format!("http://{addr}/mcp"),
    }
}

fn server_header(addr: &str, st: &graph_proto::pb::StatusResponse) -> String {
    format!(
        "server {addr} node {} (leader: {})",
        st.node_id,
        st.leader_id
            .map_or_else(|| "none".to_string(), |l| l.to_string())
    )
}

/// The server a command talks to, once resolved (for the error message).
static REMOTE_ADDR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The command line, with `serve --config <file>`'s settings applied
/// ([`graph_cli::serve_config`]).
fn parse_cli(args: Vec<std::ffi::OsString>) -> Result<Cli> {
    use clap::{CommandFactory, FromArgMatches};
    let args = graph_cli::serve_config::apply(&Cli::command(), args)?;
    let m = Cli::command().get_matches_from(args);
    Ok(Cli::from_arg_matches(&m).unwrap_or_else(|e| e.exit()))
}

/// Whether the command is a read (`describe`, `symbols`, `search`,
/// `export`): its exit-4 failure is explained as a read's.
static REMOTE_IS_READ: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn run() -> Result<i32> {
    let cli = parse_cli(std::env::args_os().collect())?;
    if let Some(d) = cli.write_deadline {
        graph_cli::target::set_write_deadline(d);
    }
    if let Some(d) = cli.read_deadline {
        graph_cli::target::set_read_deadline(d);
    }
    let _ = REMOTE_IS_READ.set(matches!(
        cli.cmd,
        Cmd::Describe { .. }
            | Cmd::Symbols { .. }
            | Cmd::Search { .. }
            | Cmd::Export { .. }
            | Cmd::Mcp
    ));
    reject_legacy_backend(cli.backend)?;
    let overrides = cli.overrides();
    // `serve` owns a file; everything else resolves --db / --server.
    if let Cmd::Serve {
        config: _,
        listen,
        data_dir,
        bootstrap,
        restore,
        restore_allow_extractor_mismatch,
        backup_url,
        backup_keep,
        backup_on,
        backup_endpoint,
        backup_region,
        backup_virtual_host,
        backup_credentials_file,
        backup_profile,
        join,
        bootstrap_or_join,
        peers,
        replicas,
        bootstrap_probe_timeout,
        force_bootstrap,
        auto_promote,
        standby: _,
        accept_snapshot_overwrite,
        join_timeout,
        advertise,
        update_advertise,
        node_id,
        node_id_from_hostname,
        log_format,
        log_level,
        metrics_listen,
        mcp_listen,
        mcp_allow_remote,
        mcp_allow_origin,
        mcp_read,
        mcp_max_inflight,
        ready_max_lag,
        snapshot_log_entries,
        snapshot_log_bytes,
        log_keep_entries,
        election_timeout_min,
        election_timeout_max,
        heartbeat_interval,
        quorum_loss_timeout,
        min_free_disk,
        snapshot_max_age,
    } = &cli.cmd
    {
        if let Some(s) = &cli.server {
            bail!("serve takes --db <file> or --data-dir <dir> to serve, not --server {s}");
        }
        graph_cli::logging::init(*log_format, log_level)?;
        let _ = JSON_LOGS.set(*log_format == graph_cli::logging::LogFormat::Json);
        if *auto_promote && join.is_none() && bootstrap_or_join.is_none() {
            bail!("--auto-promote needs --join <peer> (or --bootstrap-or-join <peer>)");
        }
        // A StatefulSet pod's identity from its host name (ADR 0004 D10,
        // docs/deploy/kubernetes.md): node id = ordinal + 1, ordinal 0
        // bootstraps, the others join.
        let host = graph_server::paths::hostname();
        let node_id = &if *node_id_from_hostname {
            Some(graph_server::paths::node_id_from_hostname(&host)?)
        } else {
            *node_id
        };
        let bootstrap_or_join = match bootstrap_or_join {
            Some(peer) => {
                let ordinal = graph_server::paths::hostname_ordinal(&host)
                    .map_err(|e| anyhow::anyhow!("--bootstrap-or-join: {e}"))?;
                let siblings = if !peers.is_empty() {
                    peers.clone()
                } else if ordinal == 0 {
                    graph_server::paths::sibling_addrs(peer, *replicas)
                        .map_err(|e| anyhow::anyhow!("--bootstrap-or-join: {e}"))?
                } else {
                    Vec::new()
                };
                Some(graph_server::paths::BootstrapOrJoin {
                    ordinal,
                    join: graph_server::JoinSpec {
                        peer: peer.clone(),
                        auto_promote: *auto_promote,
                        accept_snapshot_overwrite: *accept_snapshot_overwrite,
                        timeout: *join_timeout,
                    },
                    siblings,
                    probe_timeout: *bootstrap_probe_timeout,
                    force_bootstrap: *force_bootstrap,
                })
            }
            None => None,
        };
        if cli.chunk_bytes.is_some() {
            bail!("--chunk-bytes does not apply to serve: the server cuts replicated log entries at {} MiB", graph_client::RAFT_ENTRY_MAX_BYTES >> 20);
        }
        use std::net::ToSocketAddrs;
        let addr = listen
            .to_socket_addrs()
            .with_context(|| format!("--listen {listen}: not a host:port"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("--listen {listen}: resolves to no address"))?;
        // Either a data directory (a cluster member, stage B) or a single
        // file (stage A); `served` names it in messages.
        let (mut cfg, served, disk_path) = match data_dir {
            Some(dir) => {
                if let Some(db) = &cli.db {
                    bail!(
                        "--data-dir and --db are exclusive: --data-dir `{}` keeps its store at \
                         graph.redb inside it (drop --db `{}`)",
                        dir.display(),
                        db.display()
                    );
                }
                if dir.is_file() {
                    bail!(
                        "--data-dir `{}` is a file; give a directory (or serve the file with --db)",
                        dir.display()
                    );
                }
                let init = if let Some(b) = &bootstrap_or_join {
                    graph_server::InitMode::BootstrapOrJoin(b.clone())
                } else if *bootstrap {
                    graph_server::InitMode::Bootstrap {
                        restore: restore.clone(),
                    }
                } else if let Some(peer) = join {
                    graph_server::InitMode::Join(graph_server::JoinSpec {
                        peer: peer.clone(),
                        auto_promote: *auto_promote,
                        accept_snapshot_overwrite: *accept_snapshot_overwrite,
                        timeout: *join_timeout,
                    })
                } else {
                    graph_server::InitMode::Restart
                };
                let cfg = graph_server::ServeConfig::for_data_dir(dir, addr, init, *node_id);
                (cfg, dir.clone(), dir.clone())
            }
            None => {
                let db = cli
                    .db
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(graph_cli::target::DEFAULT_DB));
                if db.is_dir() {
                    bail!(
                        "--db `{}` is a directory; give a database file path (or serve a data \
                         directory with --data-dir)",
                        db.display()
                    );
                }
                let mut cfg = graph_server::ServeConfig::new(&db, addr);
                cfg.node_id = Some(node_id.unwrap_or(1));
                (cfg, db.clone(), db)
            }
        };
        let cluster_mode = data_dir.is_some();
        let tuned = snapshot_log_entries.is_some()
            || snapshot_log_bytes.is_some()
            || log_keep_entries.is_some()
            || election_timeout_min.is_some()
            || election_timeout_max.is_some()
            || heartbeat_interval.is_some()
            || quorum_loss_timeout.is_some();
        if tuned {
            let mut r = if cluster_mode {
                graph_server::RaftSettings::cluster()
            } else {
                graph_server::RaftSettings::standalone()
            };
            if let Some(v) = snapshot_log_entries {
                r.snapshot_log_entries = *v;
            }
            if let Some(v) = snapshot_log_bytes {
                r.snapshot_log_bytes = *v;
            }
            if let Some(v) = log_keep_entries {
                r.log_keep_entries = *v;
            }
            if let Some(v) = election_timeout_min {
                r.election_min_ms = *v;
            }
            if let Some(v) = election_timeout_max {
                r.election_max_ms = *v;
            }
            if let Some(v) = heartbeat_interval {
                r.heartbeat_ms = *v;
            }
            if let Some(v) = quorum_loss_timeout {
                r.quorum_loss_ms = Some(*v);
            }
            // In cluster mode also 3 * heartbeat < election min: the read
            // freshness lease (election min - 2 * heartbeat) must outlast
            // one heartbeat.
            if let Err(e) = r.validate(cluster_mode) {
                bail!(
                    "invalid timings (--heartbeat-interval, --election-timeout-min/-max, \
                     --quorum-loss-timeout): {e}"
                );
            }
            cfg.raft = Some(r);
        }
        // The disk guard: on by default for a data directory (a cluster
        // member keeps a log and snapshots), off by default for --db
        // (stage A behaviour); an explicit --min-free-disk applies to both.
        let min_free = match min_free_disk {
            Some(m) => Some(*m),
            None if cluster_mode => Some(graph_cli::diskinfo::MinFree::Default),
            None => None,
        };
        if let Some(m) = min_free {
            let total = graph_cli::diskinfo::sample_disk(&disk_path).map(|s| s.total);
            cfg.min_free_disk = m.resolve(total);
        }
        cfg.advertise = advertise.clone();
        cfg.update_advertise = update_advertise.clone();
        if !cluster_mode && (tuned || advertise.is_some()) {
            // Not refused (a stage A script may pass them), but said: with
            // --db they tune the one-member log of this file only.
            tracing::info!(
                "note: with --db, --advertise and the Raft options \
                 (--snapshot-log-entries/-bytes, --log-keep-entries, --election-timeout-*, \
                 --heartbeat-interval, --quorum-loss-timeout) apply to this single-node \
                 server's own log only; clusters use --data-dir"
            );
        }
        cfg.cache_bytes = cli.cache_bytes.map(|b| b as usize);
        cfg.ready_max_lag = *ready_max_lag;
        if let Some(m) = metrics_listen {
            use std::net::ToSocketAddrs;
            cfg.metrics_listen = Some(
                m.to_socket_addrs()
                    .with_context(|| format!("--metrics-listen {m}: not a host:port"))?
                    .next()
                    .ok_or_else(|| {
                        anyhow::anyhow!("--metrics-listen {m}: resolves to no address")
                    })?,
            );
        }
        if let Some(m) = mcp_listen {
            use std::net::ToSocketAddrs;
            let listen = m
                .to_socket_addrs()
                .with_context(|| format!("--mcp-listen {m}: not a host:port"))?
                .next()
                .ok_or_else(|| anyhow::anyhow!("--mcp-listen {m}: resolves to no address"))?;
            let mut mc = graph_server::mcp::McpConfig::new(listen);
            mc.allow_remote = *mcp_allow_remote;
            mc.allow_origins = mcp_allow_origin.clone();
            mc.read = match mcp_read {
                Some(McpReadArg::Linearizable) => graph_server::mcp::McpRead::Linearizable,
                _ => graph_server::mcp::McpRead::Local,
            };
            if let Some(n) = mcp_max_inflight {
                mc.max_inflight = usize::try_from(*n).unwrap_or(usize::MAX);
            }
            cfg.mcp = Some(mc);
        }
        cfg.snapshot_max_age = *snapshot_max_age;
        cfg.restore_allow_extractor_mismatch = *restore_allow_extractor_mismatch;
        let s3 = graph_server::backup::S3Options {
            endpoint: backup_endpoint.clone(),
            region: backup_region.clone(),
            virtual_host: *backup_virtual_host,
            credentials_file: backup_credentials_file.clone(),
            profile: backup_profile.clone(),
            ..Default::default()
        };
        let restore_is_s3 = restore
            .as_ref()
            .is_some_and(|r| r.to_string_lossy().starts_with("s3://"));
        cfg.restore_s3 = s3.clone();
        if let Some(url) = backup_url {
            let mut b = graph_server::backup::BackupConfig::new(url.clone());
            b.keep = *backup_keep;
            b.on = *backup_on;
            // The S3 settings serve an s3:// --restore alone when the
            // backups go to a directory.
            if !(restore_is_s3 && url.starts_with("file://")) {
                b.s3 = s3;
            }
            cfg.backup = Some(b);
        }
        cfg.sysinfo = Some(std::sync::Arc::new(server_sysinfo));
        // Test-only fault injection (serve_e2e): park writes after N
        // proposals so a test can kill the server mid-run. Not a feature.
        if let Ok(v) = std::env::var("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER") {
            cfg.testing.stall_writes_after = Some(v.trim().parse().with_context(|| {
                format!("MEMORY_GRAPH_TESTING_STALL_WRITES_AFTER={v}: not a count")
            })?);
        }
        // Test-only (serve_e2e): act as if no leader were known, so a
        // linearizable read fails with NoLeader. Not a feature.
        if std::env::var_os("MEMORY_GRAPH_TESTING_WITHHOLD_LEADER").is_some() {
            cfg.testing.withhold_leader = true;
        }
        let shown = match (cluster_mode, node_id) {
            (true, Some(n)) => format!("data dir {}, node {n}", served.display()),
            (true, None) => format!("data dir {}", served.display()),
            (false, n) => format!("db {}, node {}", served.display(), n.unwrap_or(1)),
        };
        let log_format = *log_format;
        graph_server::run_blocking_with(cfg, graph_cli::shipped_extractors(), move |r| {
            // Scripts and tests read these lines for the bound ports (the
            // metrics line first: the listening line is the start signal);
            // JSON objects under --log-format json.
            use graph_cli::logging::start_line;
            if let Some(m) = r.metrics_addr {
                let msg = format!("metrics on http://{m}/metrics");
                println!(
                    "{}",
                    start_line(log_format, "metrics", &msg, &m.to_string())
                );
            }
            if let Some(m) = r.mcp_addr {
                let msg = format!("mcp on {}", mcp_url(&m.to_string()));
                println!("{}", start_line(log_format, "mcp", &msg, &m.to_string()));
            }
            let msg = format!("listening on {} ({shown})", r.addr);
            println!(
                "{}",
                start_line(log_format, "listening", &msg, &r.addr.to_string())
            );
        })
        .map_err(|e| match e {
            StoreError::Locked(why) => graph_cli::target::locked_error(&served, &why),
            e => anyhow::Error::from(e).context(format!("serving `{}`", served.display())),
        })?;
        tracing::info!("memory-graph serve: stopped");
        return Ok(0);
    }
    let target = cli.target()?;
    let remote_addr = match &target {
        Target::Remote { addr, read } => {
            let _ = REMOTE_ADDR.set(addr.clone());
            Some((addr.clone(), *read))
        }
        Target::Embedded(_) => None,
    };
    let need_server = |what: &str| -> Result<(String, ReadMode)> {
        remote_addr.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "{what} needs --server <host:port> (or {})",
                graph_cli::target::ENV_SERVER
            )
        })
    };
    match cli.cmd {
        Cmd::Serve { .. } => unreachable!("handled above"),
        Cmd::Mcp => {
            // Stdout is the protocol channel: everything else goes to stderr.
            let backend = match &remote_addr {
                Some((addr, read)) => {
                    let store = remote(addr, *read, overrides)?;
                    let log = store.read_log();
                    eprintln!("memory-graph mcp: serving server {addr} (read {read:?}) over stdio");
                    graph_mcp::StoreBackend::remote(
                        Box::new(store),
                        Box::new(move || log.stale_reads()),
                    )
                }
                None => {
                    let store = open_existing(&target, overrides)?;
                    if let Target::Embedded(db) = &target {
                        eprintln!(
                            "memory-graph mcp: serving {} over stdio (read-only)",
                            db.display()
                        );
                    }
                    graph_mcp::StoreBackend::embedded(store)
                }
            };
            let mut server =
                graph_mcp::McpServer::new(backend, "memory-graph", env!("CARGO_PKG_VERSION"));
            graph_mcp::serve_stdio(
                &mut server,
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )
            .context("mcp stdio")?;
            eprintln!("memory-graph mcp: stdin closed, exiting");
        }
        Cmd::Health { ready } => {
            let (addr, read) = need_server("health")?;
            let service = if ready {
                graph_server::READY_SERVICE
            } else {
                ""
            };
            let r = graph_cli::target::connect_with(
                &addr,
                read,
                Some(std::time::Duration::from_secs(2)),
            )
            .and_then(|s| Ok(s.health(service)?));
            return Ok(match r {
                Ok(true) => {
                    out!("SERVING");
                    0
                }
                Ok(false) => {
                    out!("NOT_SERVING");
                    graph_cli::target::exit::NOT_SERVING
                }
                Err(e) => {
                    out!("NOT_SERVING");
                    eprintln!("{e:#}");
                    graph_cli::target::exit::NOT_SERVING
                }
            });
        }
        Cmd::Cluster { cmd } => {
            let (addr, read) = need_server("cluster")?;
            let s = remote(&addr, read, overrides)?;
            let st = s.admin_status()?;
            match cmd {
                ClusterCmd::Status { json } => {
                    if json {
                        out!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "server": addr,
                                "node_id": st.node_id,
                                "cluster_id": st.cluster_id,
                                "state": st.state,
                                "leader_id": st.leader_id,
                                "leader_addr": st.leader_addr,
                                "current_term": st.current_term,
                                "applied_index": st.applied_index,
                                "applied_term": st.applied_term,
                                "committed_index": st.committed_index,
                                "last_log_index": st.last_log_index,
                                "server_version": st.server_version,
                                "protocol_version": st.protocol_version,
                                "store_format_version": st.store_format_version,
                                "extractors_hash": st.extractors_hash,
                                "db_path": st.db_path,
                                "listen_addr": st.listen_addr,
                                "uptime_secs": st.uptime_secs,
                                "snapshot_handles": st.snapshot_handles,
                                "role": st.role,
                                "advertise": st.advertise,
                                "data_dir": st.data_dir,
                                "snapshot_index": st.snapshot_index,
                                "purged_index": st.purged_index,
                                "log_bytes": st.log_bytes,
                                "store_bytes": st.store_bytes,
                                "writes_forwarded_total": st.writes_forwarded_total,
                                "rpcs_total": st.rpcs_total,
                                "entries_applied_total": st.entries_applied_total,
                                "mcp_addr": st.mcp_addr,
                                "members": st.members.iter().map(|m| serde_json::json!({
                                    "node_id": m.node_id,
                                    "addr": m.addr,
                                    "role": m.role,
                                    "extractors_hash": m.extractors_hash,
                                })).collect::<Vec<_>>(),
                                "replication": st.replication.iter().map(|p| serde_json::json!({
                                    "node_id": p.node_id,
                                    "matched_index": p.matched_index,
                                    "lag": p.lag,
                                })).collect::<Vec<_>>(),
                                "backup": st.backup.as_ref().map(|b| serde_json::json!({
                                    "url": b.url,
                                    "on": b.on,
                                    "last_index": b.last_index,
                                    "last_success_unix": b.last_success_unix,
                                    "failures_total": b.failures_total,
                                    "bytes_total": b.bytes_total,
                                    "last_backup_error": b.last_backup_error,
                                })),
                            }))?
                        );
                    } else {
                        out!("{}", server_header(&addr, &st));
                        out!("  cluster   {}", st.cluster_id);
                        out!(
                            "  node      {} ({}) listening on {}, advertised as {}, term {}",
                            st.node_id,
                            if st.role.is_empty() {
                                &st.state
                            } else {
                                &st.role
                            },
                            st.listen_addr,
                            if st.advertise.is_empty() {
                                "-"
                            } else {
                                &st.advertise
                            },
                            st.current_term
                        );
                        out!("  mcp       {}", mcp_url(&st.mcp_addr));
                        for m in &st.members {
                            let lag = st
                                .replication
                                .iter()
                                .find(|p| p.node_id == m.node_id)
                                .map(|p| match p.matched_index {
                                    Some(i) => format!(", matched {i}, lag {}", p.lag),
                                    None => format!(", nothing matched yet, lag {}", p.lag),
                                })
                                .unwrap_or_default();
                            out!("  member    {} {} at {}{lag}", m.node_id, m.role, m.addr);
                        }
                        out!(
                            "  leader    {}",
                            match (st.leader_id, st.leader_addr.as_deref()) {
                                (Some(l), Some(a)) => format!("{l} at {a}"),
                                (Some(l), None) => l.to_string(),
                                _ => "none".into(),
                            }
                        );
                        out!(
                            "  log       applied {} (term {}), committed {}, last {}",
                            st.applied_index,
                            st.applied_term,
                            st.committed_index,
                            st.last_log_index
                        );
                        out!(
                            "  snapshot  {}, purged {}, log file {} bytes, store file {} bytes",
                            if st.snapshot_index == 0 {
                                "none".to_string()
                            } else {
                                format!("at {}", st.snapshot_index)
                            },
                            st.purged_index,
                            st.log_bytes,
                            st.store_bytes
                        );
                        out!(
                            "  server    {} (protocol {}, store format {}, extractors {})",
                            st.server_version,
                            st.protocol_version,
                            st.store_format_version,
                            st.extractors_hash
                        );
                        out!(
                            "  store     {} (up {}s, {} snapshot handles)",
                            st.db_path,
                            st.uptime_secs,
                            st.snapshot_handles
                        );
                        if let Some(b) = &st.backup {
                            out!(
                                "  backup    {} (on {}): last {}, {} failed, {} bytes",
                                b.url,
                                b.on,
                                if b.last_index == 0 {
                                    "none yet".to_string()
                                } else {
                                    format!("at {} (unix {})", b.last_index, b.last_success_unix)
                                },
                                b.failures_total,
                                b.bytes_total
                            );
                            if !b.last_backup_error.is_empty() {
                                out!("  backup    last error: {}", b.last_backup_error);
                            }
                        }
                    }
                }
                ClusterCmd::Leader { json } => {
                    let Some(leader) = st.leader_id else {
                        if json {
                            out!("{}", serde_json::json!({ "leader_id": null }));
                        }
                        eprintln!("no leader is known to server {addr}");
                        return Ok(graph_cli::target::exit::NO_LEADER);
                    };
                    if json {
                        out!(
                            "{}",
                            serde_json::json!({ "leader_id": leader, "leader_addr": st.leader_addr })
                        );
                    } else {
                        out!("{leader} {}", st.leader_addr.as_deref().unwrap_or("-"));
                    }
                }
                ClusterCmd::Snapshot {
                    upload: true, json, ..
                } => {
                    let r = s
                        .admin_upload_snapshot()
                        .context("cluster snapshot --upload")?;
                    let b = r.backup.unwrap_or_default();
                    if json {
                        out!(
                            "{}",
                            serde_json::json!({
                                "node_id": r.node_id,
                                "url": b.url,
                                "term": b.term,
                                "index": b.index,
                                "size": b.size,
                                "sha256": b.sha256,
                                "extractors_hash": b.extractors_hash,
                                "store_format_version": b.store_format_version,
                            })
                        );
                    } else {
                        out!(
                            "uploaded snapshot at log index {} (term {}) from node {}: {} bytes, \
                             sha256 {}",
                            b.index,
                            b.term,
                            r.node_id,
                            b.size,
                            b.sha256
                        );
                        out!("  {}", b.url);
                    }
                }
                ClusterCmd::Backups { json } => {
                    let r = s.admin_list_backups().context("cluster backups")?;
                    if json {
                        let list: Vec<_> = r
                            .backups
                            .iter()
                            .map(|b| {
                                serde_json::json!({
                                    "url": b.url,
                                    "term": b.term,
                                    "index": b.index,
                                    "size": b.size,
                                    "sha256": b.sha256,
                                    "extractors_hash": b.extractors_hash,
                                    "store_format_version": b.store_format_version,
                                    "error": (!b.error.is_empty()).then_some(&b.error),
                                })
                            })
                            .collect();
                        out!(
                            "{}",
                            serde_json::json!({
                                "location": r.location,
                                "cluster_id": r.cluster_id,
                                "backups": list,
                            })
                        );
                    } else {
                        out!(
                            "{} backup(s) of cluster {} in {}",
                            r.backups.len(),
                            r.cluster_id,
                            r.location
                        );
                        for b in &r.backups {
                            if b.error.is_empty() {
                                out!(
                                    "  index {} term {}: {} bytes, sha256 {}\n    {}",
                                    b.index,
                                    b.term,
                                    b.size,
                                    b.sha256,
                                    b.url
                                );
                            } else {
                                out!(
                                    "  index {} term {}: unreadable .meta ({})\n    {}",
                                    b.index,
                                    b.term,
                                    b.error,
                                    b.url
                                );
                            }
                        }
                    }
                }
                ClusterCmd::Snapshot { out, json, .. } => {
                    let info = s
                        .admin_trigger_snapshot(out.as_deref())
                        .context("cluster snapshot")?;
                    if json {
                        out!(
                            "{}",
                            serde_json::json!({
                                "node_id": st.node_id,
                                "last_applied_index": info.last_applied_index,
                                "last_applied_term": info.last_applied_term,
                                "size": info.size,
                                "sha256": info.sha256,
                                "extractors_hash": info.extractors_hash,
                                "store_format_version": info.store_format_version,
                                "out": out.as_ref().map(|p| p.display().to_string()),
                            })
                        );
                    } else {
                        out!(
                            "snapshot of node {} at log index {} (term {}): {} bytes, sha256 {}",
                            st.node_id,
                            info.last_applied_index,
                            info.last_applied_term,
                            info.size,
                            info.sha256
                        );
                        if let Some(p) = &out {
                            out!("  written to {} (sha256 and size verified)", p.display());
                        }
                    }
                }
                ClusterCmd::Members { json } => {
                    let m = s.admin_members().context("cluster members")?;
                    if json {
                        out!(
                            "{}",
                            serde_json::json!({
                                "server": addr,
                                "leader_id": m.leader_id,
                                "members": m.members.iter().map(|x| serde_json::json!({
                                    "node_id": x.node_id,
                                    "addr": x.addr,
                                    "role": x.role,
                                    "leader": Some(x.node_id) == m.leader_id,
                                })).collect::<Vec<_>>(),
                            })
                        );
                    } else {
                        for x in &m.members {
                            let lead = if Some(x.node_id) == m.leader_id {
                                "  (leader)"
                            } else {
                                ""
                            };
                            out!("{} {} {}{lead}", x.node_id, x.role, x.addr);
                        }
                    }
                }
                ClusterCmd::AddLearner {
                    id,
                    addr: at,
                    no_wait,
                } => {
                    let index = s
                        .admin_add_learner(id, &at, !no_wait)
                        .with_context(|| format!("cluster add-learner {id} {at}"))?;
                    out!("node {id} at {at} added as a learner (log index {index})");
                }
                ClusterCmd::Promote { id } => {
                    let index = s
                        .admin_promote(id)
                        .with_context(|| format!("cluster promote {id}"))?;
                    out!("node {id} is now a voter (log index {index})");
                }
                ClusterCmd::Remove { id, force } => {
                    let r = s
                        .admin_remove(id, force)
                        .with_context(|| format!("cluster remove {id}"))?;
                    if !r.not_a_member {
                        out!(
                            "node {id} was removed from the cluster (log index {})",
                            r.log_index
                        );
                    } else if r.retried {
                        // The first attempt may have removed it before its
                        // answer was lost.
                        out!(
                            "node {id} is not a member (now); it may have been removed by an \
                             earlier attempt of this command"
                        );
                    } else {
                        out!("node {id} is not a member; nothing to remove");
                    }
                }
                ClusterCmd::TransferLeader { id } => {
                    let leader = s
                        .admin_transfer_leader(id)
                        .with_context(|| format!("cluster transfer-leader {id}"))?;
                    out!("node {leader} is now the leader");
                }
            }
        }
        Cmd::IndexFile {
            org,
            repo,
            language,
            reindex,
            path,
        } => {
            if let Ok(m) = std::fs::metadata(&path) {
                if let Some(reason) = graph_cli::size_skip_reason(m.len(), None) {
                    bail!("`{}` is {reason}", path.display());
                }
            }
            let bytes = std::fs::read(&path)
                .with_context(|| format!("cannot read `{}`", path.display()))?;
            let path_str = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("`{}` is not a valid UTF-8 path", path.display()))?;
            if let Target::Embedded(db) = &target {
                if db.is_dir() {
                    bail!(
                        "--db `{}` is a directory; give a database file path",
                        db.display()
                    );
                }
            }
            let store = open_for_indexing(&target, overrides)?;
            graph_cli::warn_extractor_gaps(&*store, &org, &repo);
            let st = store.index_bytes_opts(
                &org,
                &repo,
                path_str,
                &bytes,
                language.as_deref(),
                None,
                IndexOptions { reindex },
            )?;
            let lang = st.language.clone();
            out!(
                "indexed {} ({}) tokens={} symbols={}{}{}{}",
                path.display(),
                lang,
                st.tokens,
                st.symbols,
                if st.replaced { " [replaced]" } else { "" },
                if st.unchanged { " [unchanged]" } else { "" },
                if st.has_errors { " [has_errors]" } else { "" }
            );
        }
        Cmd::Index {
            org,
            repo,
            json,
            max_file_size,
            prune,
            force,
            reindex,
            jobs,
            memory,
            deterministic,
            stats,
            trace,
            progress,
            no_progress,
            min_free_disk,
            no_disk_check,
            dir,
        } => {
            // With a server: connect first (its leader and applied index
            // feed the progress board), then hand the connection over.
            let (db, remote_store, remote_board) = match &target {
                Target::Embedded(db) => (db.clone(), None, None),
                Target::Remote { addr, read } => {
                    let s = remote(addr, *read, overrides)?;
                    if jobs != 0 {
                        eprintln!(
                            "warning: with --server, --jobs sizes the threads that read and send files; the server parses them"
                        );
                    }
                    let board = graph_cli::progress::RemoteBoard::new(
                        addr,
                        s.hello().leader_id,
                        s.applied_index(),
                        s.forwarded_to_leader(),
                    );
                    (PathBuf::new(), Some(s), Some(board))
                }
            };
            let mut remote_store = remote_store;
            index_dir(
                DirOpts {
                    db: &db,
                    org: &org,
                    repo: &repo,
                    dir: &dir,
                    json,
                    max_file_size,
                    prune,
                    force,
                    reindex,
                    jobs,
                    memory,
                    deterministic,
                    stats,
                    trace: trace.as_deref(),
                    progress: match (progress, no_progress) {
                        (true, _) => Some(true),
                        (_, true) => Some(false),
                        _ => None,
                    },
                    disk_probe: None,
                    min_free_disk: min_free_disk.unwrap_or(graph_cli::diskinfo::MinFree::Default),
                    disk_check: !no_disk_check,
                    chunk_bytes: cli.chunk_bytes.unwrap_or(graph_cli::DEFAULT_CHUNK_BYTES),
                    remote: remote_board,
                },
                |_| match remote_store.take() {
                    Some(s) => Ok(Box::new(s) as Box<dyn Store>),
                    None => open_for_indexing(&target, overrides),
                },
                &mut std::io::stdout().lock(),
            )?
        }
        Cmd::Sysinfo {
            json,
            memory,
            min_free_disk,
        } => match &target {
            Target::Remote { addr, read } => {
                // The server machine's report: that is where indexing runs.
                let s = remote(addr, *read, overrides)?;
                let st = s.admin_status()?;
                let mut report: serde_json::Value = serde_json::from_str(&s.admin_sysinfo()?)
                    .context("the server's sysinfo report is not JSON")?;
                let text = report
                    .as_object_mut()
                    .and_then(|o| o.remove("text"))
                    .and_then(|t| t.as_str().map(str::to_string));
                if json {
                    let doc = serde_json::json!({
                        "server": addr,
                        "node_id": st.node_id,
                        "leader_id": st.leader_id,
                        "report": report,
                    });
                    out!("{}", serde_json::to_string_pretty(&doc)?);
                } else {
                    out!("{}", server_header(addr, &st));
                    match text {
                        Some(t) => write!(std::io::stdout().lock(), "{t}")?,
                        None => out!("{}", serde_json::to_string_pretty(&report)?),
                    }
                }
            }
            Target::Embedded(db) => {
                let r = graph_cli::report::Report::detect(
                    db,
                    memory,
                    min_free_disk.unwrap_or(graph_cli::diskinfo::MinFree::Default),
                );
                if json {
                    out!("{}", serde_json::to_string_pretty(&r.json())?);
                } else {
                    write!(std::io::stdout().lock(), "{}", r.text())?;
                }
            }
        },
        Cmd::Vacuum { compact } => {
            let (vst, cst) = match &target {
                Target::Remote { addr, read } => {
                    let s = remote(addr, *read, overrides)?;
                    let vst = s.vacuum()?;
                    // The server's own vacuum --compact (`Admin.Compact`).
                    let cst = if compact {
                        Some(s.admin_compact()?)
                    } else {
                        None
                    };
                    (vst, cst)
                }
                Target::Embedded(db) => {
                    if !db.is_file() {
                        bail!("database `{}` does not exist", db.display());
                    }
                    // `compact` is `V2Store`-only (it consumes and replaces
                    // `self`, which `Store`'s `&self`-only shape can't
                    // express), so this needs a concrete, owned `V2Store`
                    // rather than the `Box<dyn Store>` the other arms use.
                    let s = graph_cli::target::open_embedded(db, || open_v2(db, overrides))
                        .with_context(|| format!("opening database `{}`", db.display()))?;
                    let vst = s.vacuum()?;
                    let cst = if compact { Some(s.compact()?.1) } else { None };
                    (vst, cst)
                }
            };
            out!(
                "vacuum: removed {} unused dictionary terms, kept {}",
                vst.terms_removed,
                vst.terms_kept
            );
            if let Some(cst) = cst {
                out!(
                    "compact: {} bytes -> {} bytes",
                    cst.before_bytes,
                    cst.after_bytes
                );
            }
        }
        Cmd::Describe { org, repo, json } => {
            let store = open_existing(&target, overrides)?;
            let infos = store.describe(org.as_deref(), repo.as_deref())?;
            if json {
                out!(
                    "{}",
                    serde_json::to_string(&graph_cli::target::with_read_meta(
                        serde_json::json!({ "repos": infos })
                    ))?
                );
            } else {
                for i in &infos {
                    out!("{}/{}: {} files", i.org, i.repo, i.files);
                    if i.open_batch {
                        out!(
                            "  WARNING: repo {}/{} has an incomplete ingest batch \
                             (crashed or in-progress); re-index to repair",
                            i.org,
                            i.repo
                        );
                    }
                    for (l, li) in &i.languages {
                        out!(
                            "  {l}: {} files, {} symbols, {} tokens",
                            li.files,
                            li.symbols,
                            li.tokens
                        );
                        for (k, n) in &li.symbol_kinds {
                            out!("    {k}: {n}");
                        }
                    }
                }
            }
        }
        Cmd::Symbols {
            pattern,
            kind,
            language,
            org,
            repo,
            file,
            limit,
            offset,
            json,
        } => {
            let store = open_existing(&target, overrides)?;
            validate_filters(
                &*store,
                org.as_deref(),
                repo.as_deref(),
                language.as_deref(),
                kind.as_deref(),
            )?;
            let mut q = SymbolQuery::new(&pattern);
            (q.kind, q.language, q.org, q.repo, q.file) = (kind, language, org, repo, file);
            q.limit = limit.map(|l| l as usize);
            q.offset = offset.map(|o| o as usize);
            let hits = store.search_symbols(&q)?;
            if json {
                let out = graph_cli::target::with_read_meta(
                    serde_json::json!({ "query": pattern, "results": hits }),
                );
                out!("{}", serde_json::to_string(&out)?);
            } else {
                for h in &hits {
                    let loc = h
                        .span
                        .map(|s| format!(":{}:{}", s.start_line, s.start_col))
                        .unwrap_or_default();
                    out!(
                        "{}/{}/{}{loc}\t{}\t{} ({})\t{}",
                        h.org,
                        h.repo,
                        h.file,
                        h.language.as_deref().unwrap_or("-"),
                        h.kind.as_str(),
                        h.lang_kind.as_deref().unwrap_or("-"),
                        h.qualified
                    );
                }
            }
        }
        Cmd::Export { out } => {
            let store = open_existing(&target, overrides)?;
            let mut file_writer;
            let mut stdout_writer;
            let writer: &mut dyn std::io::Write = match &out {
                Some(p) => {
                    file_writer = std::io::BufWriter::new(
                        std::fs::File::create(p)
                            .with_context(|| format!("creating `{}`", p.display()))?,
                    );
                    &mut file_writer
                }
                None => {
                    stdout_writer = std::io::stdout().lock();
                    &mut stdout_writer
                }
            };
            let n = graph_store::export::export_ndjson(store.as_ref(), writer)?;
            if out.is_some() {
                out!("exported {n} nodes");
            }
        }
        Cmd::Search {
            text,
            language,
            org,
            repo,
            kind,
            grain,
            symbol_kind,
            limit,
            offset,
            json,
        } => {
            if symbol_kind.is_some() && !grain.is_symbolic() {
                bail!("--symbol-kind requires --grain symbol, method or class");
            }
            // A generic kind the grain can never accept would only ever give
            // `no_matching_symbol` rows: refuse it, like a typo. Language-
            // specific kinds are checked against `describe` below.
            if let Some(k) = symbol_kind.as_deref() {
                use graph_core::SymbolKind as K;
                if let Ok(g) = k.parse::<K>() {
                    let (fits, allowed) = match grain {
                        Grain::Method => {
                            (matches!(g, K::Method | K::Function), "method or function")
                        }
                        Grain::Class => (matches!(g, K::Type | K::Other), "type or other"),
                        _ => (true, ""),
                    };
                    if !fits {
                        bail!(
                            "--symbol-kind {k} can never be a --grain {} row (generic kinds there: {allowed}); use --grain symbol for any kind",
                            format!("{grain:?}").to_lowercase()
                        );
                    }
                }
            }
            let store = open_existing(&target, overrides)?;
            validate_filters(
                &*store,
                org.as_deref(),
                repo.as_deref(),
                language.as_deref(),
                symbol_kind.as_deref(),
            )?;
            let mut q = Query::new(&text);
            (q.language, q.org, q.repo, q.class, q.grain, q.symbol_kind) =
                (language, org, repo, kind, grain, symbol_kind);
            q.limit = limit.map(|l| l as usize);
            q.offset = offset.map(|o| o as usize);
            let hits = store.search(&q)?;
            if json {
                let out = graph_cli::target::with_read_meta(
                    serde_json::json!({ "query": text, "grain": grain, "results": hits }),
                );
                out!("{}", serde_json::to_string(&out)?);
            } else {
                for h in &hits {
                    // A symbol row is a whole definition: show where it ends too.
                    let loc = h
                        .span
                        .map(|s| {
                            if h.grain.is_symbolic() {
                                format!(
                                    ":{}:{}-{}:{}",
                                    s.start_line, s.start_col, s.end_line, s.end_col
                                )
                            } else {
                                format!(":{}:{}", s.start_line, s.start_col)
                            }
                        })
                        .unwrap_or_default();
                    let path = [Some(h.org.as_str()), h.repo.as_deref(), h.file.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("/");
                    out!(
                        "{path}{loc}\t{}\t{}\thits={}{}",
                        h.language.as_deref().unwrap_or("-"),
                        h.symbol.as_deref().unwrap_or("-"),
                        h.count,
                        if h.no_symbols {
                            "\tno_symbols"
                        } else if h.no_matching_symbol {
                            "\tno_matching_symbol"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod legacy_backend_flag_tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `--backend v1` must never silently become a no-op: the retired
    /// format is refused with the migration hint. `--backend v2` and no flag
    /// are accepted, and other values are rejected by clap.
    #[test]
    fn backend_v1_is_refused_with_the_migration_hint() {
        let err = reject_legacy_backend(Some(LegacyBackend::V1))
            .unwrap_err()
            .to_string();
        for needle in ["retired", "v1-last", "Re-index", "memory-graph migrate"] {
            assert!(err.contains(needle), "{needle}: {err}");
        }
        reject_legacy_backend(Some(LegacyBackend::V2)).unwrap();
        reject_legacy_backend(None).unwrap();
        let cli = parse(&["memory-graph", "--backend", "v1", "describe"]);
        assert_eq!(cli.backend, Some(LegacyBackend::V1));
        assert!(reject_legacy_backend(cli.backend).is_err());
        let e = Cli::try_parse_from(["memory-graph", "--backend", "v3", "describe"])
            .err()
            .expect("v3 is rejected");
        assert!(e.to_string().contains("invalid value 'v3'"), "{e}");
    }

    /// The old flag names stay usable as hidden aliases.
    #[test]
    fn old_v2_flag_names_are_aliases() {
        let cli = parse(&[
            "memory-graph",
            "--v2-chunk-bytes",
            "7",
            "--v2-cache-bytes",
            "9",
            "describe",
        ]);
        assert_eq!((cli.chunk_bytes, cli.cache_bytes), (Some(7), Some(9)));
        let cli = parse(&[
            "memory-graph",
            "--chunk-bytes",
            "7",
            "--cache-bytes",
            "9",
            "describe",
        ]);
        assert_eq!((cli.chunk_bytes, cli.cache_bytes), (Some(7), Some(9)));
    }
}

#[cfg(test)]
mod chunk_bytes_flag_tests {
    use super::*;
    use graph_core::{Extraction, Span, SymbolDecl, SymbolKind};
    use graph_store::BatchFile;

    /// Same technique as `chunked_batches_are_atomic_per_chunk`
    /// (`crates/graph-store/src/v2_policy_tests.rs`): a symbol `lang_kind`
    /// containing a NUL makes storage reject that one file, poisoning
    /// whichever chunk it lands in.
    struct Poison;
    impl Extractor for Poison {
        fn language(&self) -> &str {
            "poison"
        }
        fn extract(&self, source: &str) -> Extraction {
            let span = Span {
                start: 0,
                end: source.len() as u32,
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: source.len() as u32 + 1,
            };
            let mut ex = Extraction {
                has_errors: false,
                symbols: vec![SymbolDecl {
                    owner: None,
                    name: "S".into(),
                    kind: SymbolKind::Function,
                    lang_kind: None,
                    span,
                }],
                tokens: vec![],
            };
            if source.contains("BAD") {
                ex.symbols[0].lang_kind = Some("nul\0".into());
            }
            ex
        }
    }

    fn batch<'a>(srcs: &'a [String], paths: &'a [String]) -> Vec<BatchFile<'a>> {
        srcs.iter()
            .zip(paths)
            .map(|(s, p)| BatchFile {
                path: p,
                bytes: s.as_bytes(),
                language: Some("poison"),
                origin: None,
            })
            .collect()
    }

    /// Issue #28: `--chunk-bytes` is plumbed through `open_with_overrides`
    /// (the function `open_for_indexing` calls for `index`/`index-file`) into
    /// `V2Store::set_chunk_bytes`. A prior e2e test only checked that search
    /// results were identical chunked vs. unchunked, which is true by design
    /// regardless of whether chunking actually happened -- QA confirmed by
    /// mutation that silently dropping the `set_chunk_bytes` call left the
    /// suite green.
    ///
    /// This test calls `open_with_overrides` directly -- the exact function
    /// the CLI's flag parsing invokes once `--chunk-bytes` is parsed into
    /// `Overrides` -- then runs a batch with one poisoned file through it
    /// (mirroring `chunked_batches_are_atomic_per_chunk`): a small chunk cap
    /// must commit the chunks before the failure, while the default
    /// (unchunked) cap commits nothing, since the whole batch is one
    /// transaction. If the override became a no-op, both runs would behave
    /// like the default cap and report the same file count.
    #[test]
    fn chunk_bytes_override_changes_commit_granularity() {
        let d = tempfile::tempdir().unwrap();
        // Each source is 10 bytes; f2 is poisoned.
        let srcs: Vec<String> = ["aaaaaaaaaa", "bbbbbbbbbb", "BADcccccc!", "dddddddddd"]
            .map(String::from)
            .to_vec();
        let paths: Vec<String> = (0..4).map(|i| format!("f{i}.p")).collect();
        let opts = graph_store::IndexOptions::default();

        // Cap of 20 bytes: {f0, f1} commit as one chunk, then f2 fails and
        // only its own chunk is lost; f3 is never reached.
        let small = open_with_overrides(
            &d.path().join("small.redb"),
            vec![Box::new(Poison)],
            Overrides {
                chunk_bytes: Some(20),
                cache_bytes: None,
            },
        )
        .unwrap();
        assert!(small
            .index_batch("o", "r", &batch(&srcs, &paths), opts)
            .is_err());
        let files_small: usize = small
            .describe(None, None)
            .unwrap()
            .iter()
            .map(|r| r.files)
            .sum();
        assert_eq!(
            files_small, 2,
            "small --chunk-bytes: earlier chunks stay committed"
        );

        // No override: the whole batch is one transaction, all or nothing.
        let big = open_with_overrides(
            &d.path().join("big.redb"),
            vec![Box::new(Poison)],
            Overrides {
                chunk_bytes: None,
                cache_bytes: None,
            },
        )
        .unwrap();
        assert!(big
            .index_batch("o", "r", &batch(&srcs, &paths), opts)
            .is_err());
        let files_big: usize = big
            .describe(None, None)
            .unwrap()
            .iter()
            .map(|r| r.files)
            .sum();
        assert_eq!(
            files_big, 0,
            "no --chunk-bytes override: one chunk, nothing stored"
        );

        assert_ne!(
            files_small, files_big,
            "the --chunk-bytes override must change commit granularity, \
             or the flag has become a silent no-op (issue #28)"
        );
    }
}

/// `serve --config <file>` (epic story 25 AC 4, issue #106): a file with
/// every setting gives exactly what the same flags give.
#[cfg(test)]
mod serve_config_tests {
    use super::*;
    use clap::CommandFactory;
    use std::ffi::OsString;

    /// Every serve (and global) argument's raw values, by id, as parsed.
    fn parsed(args: Vec<OsString>) -> Vec<(String, Vec<OsString>)> {
        let args = graph_cli::serve_config::apply(&Cli::command(), args).unwrap();
        let top = Cli::command().try_get_matches_from(args).unwrap();
        let (name, sm) = top.subcommand().unwrap();
        assert_eq!(name, "serve");
        let cmd = Cli::command();
        let serve = cmd.find_subcommand("serve").unwrap();
        let ids: Vec<String> = serve
            .get_arguments()
            .chain(cmd.get_arguments())
            .map(|a| a.get_id().to_string())
            .filter(|id| id != "config" && id != "help")
            .collect();
        ids.into_iter()
            .map(|id| {
                let v = sm
                    .try_get_raw(&id)
                    .ok()
                    .flatten()
                    .map(|v| v.map(OsString::from).collect())
                    .unwrap_or_default();
                (id, v)
            })
            .collect()
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn check(flags: &[&str], toml: &str) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("serve.toml");
        std::fs::write(&f, toml).unwrap();
        let from_flags = parsed(os(flags));
        let mut a = os(&["memory-graph", "serve", "--config"]);
        a.push(f.into_os_string());
        let from_file = parsed(a);
        assert_eq!(from_file, from_flags);
        // And the file really set something (not two sets of defaults).
        let listen = from_file.iter().find(|(id, _)| id == "listen").unwrap();
        assert_ne!(listen.1, os(&["127.0.0.1:7000"]));
    }

    #[test]
    fn a_config_file_equals_the_same_flags() {
        check(
            &[
                "memory-graph",
                "serve",
                "--data-dir",
                "/data",
                "--join",
                "peer:7000",
                "--auto-promote",
                "--accept-snapshot-overwrite",
                "--join-timeout",
                "30s",
                "--listen",
                "0.0.0.0:7001",
                "--advertise",
                "h:7001",
                "--node-id",
                "3",
                "--log-format",
                "json",
                "--log-level",
                "debug",
                "--metrics-listen",
                "0.0.0.0:9100",
                "--mcp-listen",
                "0.0.0.0:7071",
                "--mcp-allow-remote",
                "--mcp-allow-origin",
                "http://localhost:6274",
                "--mcp-allow-origin",
                "https://a.example",
                "--mcp-read",
                "linearizable",
                "--mcp-max-inflight",
                "4",
                "--ready-max-lag",
                "50",
                "--snapshot-log-entries",
                "500",
                "--snapshot-log-bytes",
                "64M",
                "--log-keep-entries",
                "10",
                "--election-timeout-min",
                "600",
                "--election-timeout-max",
                "1200",
                "--heartbeat-interval",
                "100",
                "--quorum-loss-timeout",
                "1500",
                "--min-free-disk",
                "5%",
                "--snapshot-max-age",
                "10m",
                "--cache-bytes",
                "1000000",
                "--backup-url",
                "file:///srv/backups",
                "--backup-keep",
                "3",
                "--backup-on",
                "all",
                "--backup-endpoint",
                "http://minio:9000",
                "--backup-region",
                "eu-west-1",
                "--backup-virtual-host",
                "--backup-credentials-file",
                "/etc/mg/aws",
                "--backup-profile",
                "backup",
            ],
            r#"
backup-url = "file:///srv/backups"
backup-endpoint = "http://minio:9000"
backup_region = "eu-west-1"
backup-virtual-host = true
backup-credentials-file = "/etc/mg/aws"
backup-profile = "backup"
backup_keep = 3
backup-on = "all"
data-dir = "/data"
join = "peer:7000"
auto_promote = true
accept-snapshot-overwrite = true
join-timeout = "30s"
listen = "0.0.0.0:7001"
advertise = "h:7001"
node-id = 3
log-format = "json"
log_level = "debug"
metrics-listen = "0.0.0.0:9100"
mcp-listen = "0.0.0.0:7071"
mcp_allow_remote = true
mcp-allow-origin = ["http://localhost:6274", "https://a.example"]
mcp-read = "linearizable"
mcp-max-inflight = 4
ready-max-lag = 50
snapshot-log-entries = 500
snapshot-log-bytes = "64M"
log-keep-entries = 10
election-timeout-min = 600
election-timeout-max = 1200
heartbeat-interval = 100
quorum-loss-timeout = 1500
min-free-disk = "5%"
snapshot-max-age = "10m"
cache-bytes = 1000000
"#,
        );
        check(
            &[
                "memory-graph",
                "serve",
                "--data-dir",
                "/data",
                "--bootstrap-or-join",
                "mg-0.mg:7000",
                "--peers",
                "mg-1.mg:7000,mg-2.mg:7000",
                "--replicas",
                "5",
                "--bootstrap-probe-timeout",
                "10s",
                "--force-bootstrap",
                "--node-id-from-hostname",
                "--listen",
                "0.0.0.0:7000",
            ],
            r#"
data_dir = "/data"
bootstrap_or_join = "mg-0.mg:7000"
peers = ["mg-1.mg:7000", "mg-2.mg:7000"]
replicas = 5
bootstrap_probe_timeout = "10s"
force_bootstrap = true
node_id_from_hostname = true
listen = "0.0.0.0:7000"
"#,
        );
        check(
            &[
                "memory-graph",
                "serve",
                "--data-dir",
                "/data",
                "--update-advertise",
                "new:7000",
                "--listen",
                "0.0.0.0:7002",
            ],
            "data-dir = \"/data\"\nupdate-advertise = \"new:7000\"\nlisten = \"0.0.0.0:7002\"\n",
        );
        check(
            &[
                "memory-graph",
                "serve",
                "--db",
                "g.redb",
                "--listen",
                "127.0.0.1:0",
            ],
            "db = \"g.redb\"\nlisten = \"127.0.0.1:0\"\n",
        );
    }

    #[test]
    fn flags_override_the_config_file_and_clap_still_validates_it() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("serve.toml");
        std::fs::write(
            &f,
            "listen = \"0.0.0.0:7001\"\nnode-id = 3\ndata-dir = \"/d\"\n",
        )
        .unwrap();
        let mut a = os(&["memory-graph", "serve", "--node-id", "5", "--config"]);
        a.push(f.clone().into_os_string());
        let got = parsed(a);
        let get = |id: &str| got.iter().find(|(i, _)| i == id).unwrap().1.clone();
        assert_eq!(get("node_id"), os(&["5"]));
        assert_eq!(get("listen"), os(&["0.0.0.0:7001"]));
        // The file's values go through the flags' own rules: `bootstrap`
        // needs a data directory.
        std::fs::write(&f, "bootstrap = true\n").unwrap();
        let mut a = os(&["memory-graph", "serve", "--config"]);
        a.push(f.into_os_string());
        let args = graph_cli::serve_config::apply(&Cli::command(), a).unwrap();
        assert!(Cli::command().try_get_matches_from(args).is_err());
    }

    /// A secret in the config file is refused (ADR 0006 E6), in whatever
    /// spelling, and never echoed.
    #[test]
    fn a_secret_in_the_config_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("serve.toml");
        for key in [
            "aws_secret_access_key",
            "aws-access-key-id",
            "aws_session_token",
            "backup-secret",
            "backup_access_key",
            "backup-password",
        ] {
            std::fs::write(
                &f,
                format!("backup-url = \"s3://b/p\"\n{key} = \"hunter2-SECRET\"\n"),
            )
            .unwrap();
            let mut a = os(&["memory-graph", "serve", "--config"]);
            a.push(f.clone().into_os_string());
            let e = format!(
                "{:#}",
                graph_cli::serve_config::apply(&Cli::command(), a).unwrap_err()
            );
            assert!(e.contains("secrets are never read"), "{key}: {e}");
            assert!(!e.contains("hunter2-SECRET"), "{key}: {e}");
        }
    }

    /// QA 1: a flag on the command line whose `requires` target (the data
    /// directory) comes from the file: the command line alone is incomplete,
    /// the merged whole is valid and equals the same flags typed.
    #[test]
    fn a_command_line_flag_may_need_what_the_file_gives() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("serve.toml");
        std::fs::write(
            &f,
            "data-dir = \"/data\"\nnode-id = 3\nlisten = \"0.0.0.0:7003\"\n",
        )
        .unwrap();
        let base = [
            "--data-dir",
            "/data",
            "--node-id",
            "3",
            "--listen",
            "0.0.0.0:7003",
        ];
        for extra in [
            &["--bootstrap"][..],
            &["--join", "peer:7000"][..],
            &["--update-advertise", "new:7003"][..],
        ] {
            let mut a = os(&["memory-graph", "serve"]);
            a.extend(os(extra));
            // Alone, clap refuses it (the data directory is missing).
            assert!(
                Cli::command().try_get_matches_from(a.clone()).is_err(),
                "{extra:?}"
            );
            a.push("--config".into());
            a.push(f.clone().into_os_string());
            let mut typed = os(&["memory-graph", "serve"]);
            typed.extend(os(&base));
            typed.extend(os(extra));
            assert_eq!(parsed(a), parsed(typed), "{extra:?}");
        }
    }
}

#[cfg(test)]
mod help_text_tests {
    use super::*;
    use clap::CommandFactory;

    /// Every visible argument of every (sub)command, `--help` and `--version`
    /// aside, says what it is for (#98).
    #[test]
    fn no_argument_has_empty_help() {
        fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if arg.is_hide_set() || id == "help" || id == "version" {
                    continue;
                }
                let empty = arg
                    .get_help()
                    .map(|h| h.to_string().trim().is_empty())
                    .unwrap_or(true);
                if empty {
                    missing.push(format!("{path} {id}"));
                }
            }
            for sub in cmd.get_subcommands() {
                if sub.is_hide_set() || sub.get_name() == "help" {
                    continue;
                }
                walk(sub, &format!("{path} {}", sub.get_name()), missing);
            }
        }
        let mut cmd = Cli::command();
        cmd.build();
        let mut missing = vec![];
        walk(&cmd, "memory-graph", &mut missing);
        assert!(missing.is_empty(), "arguments without help: {missing:#?}");
    }
}
