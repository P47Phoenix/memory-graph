# Docker

The image `ghcr.io/p47phoenix/memory-graph` is the CLI as a static binary on an empty base (`FROM scratch`): no shell, no package manager, 7 MB, built for `linux/amd64` and `linux/arm64`. Every CLI command and flag works unchanged inside it. Three things to know:

- It runs as user `65532`, not root.
- Its working directory is `/data`, so the default `--db ./graph.redb` lands on whatever you mount at `/data`. Mount the same volume on every run and the commands share one database.
- Always give a tag. `latest` only exists once a release has been tagged (see [Tags and releases](#tags-and-releases)); until then use `:main`.

## First run

Pull the image, index a project into a named volume, then query it. The source is mounted read-only at `/src`; the database lives in the `mg-data` volume.

```sh
docker pull ghcr.io/p47phoenix/memory-graph:main
cd ~/code/api
docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main describe
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main search foo --language rust
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main search foo --grain method --json
```

The `index` line prints the same summary as the native binary (`indexed acme/api: files=... symbols=... tokens=...`). Re-run it after editing the code: unchanged files are skipped. To index a second project into the same database, run `index` again with another `--repo` (or `--org`) and a different source mount; `describe` then lists both.

To see the live progress view (one line per pipeline stage) give the container a terminal with `-t`; without it only the final summary is printed.

The image has no shell, so there is nothing to `docker exec` into and `--entrypoint /bin/sh` fails. Everything is done through the `memory-graph` entrypoint, and each command exits when done.

## Windows

PowerShell: same commands, with `${PWD}` for the current directory.

```powershell
docker run --rm -v "${PWD}:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
docker run --rm -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main describe
```

Git Bash rewrites container paths such as `/src` and `/data` into Windows paths (the error reads `` `C:/Program Files/Git/src` is not a directory``, and a bind-mounted `/data` leaves a stray `db;C` directory behind). Turn that off for every `docker run` that mounts something, including the alias below: prefix the command, or export the variable once for the shell.

```sh
MSYS_NO_PATHCONV=1 docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
export MSYS_NO_PATHCONV=1     # or once per shell
```

## Keeping the database in a host directory

A fresh named volume is writable by the image's user as is. To keep the database in a directory you can see, bind-mount it and run as its owner; on Linux and macOS that is `--user "$(id -u):$(id -g)"`. On Docker Desktop (Windows, macOS) a bind mount is writable without `--user`; in Git Bash add the `MSYS_NO_PATHCONV=1` prefix from the [Windows](#windows) section. Keep the directory outside the source tree, or the database file shows up in the index summary as `skipped (database file)`.

```sh
mkdir -p ~/mg-db
docker run --rm -v "$PWD:/src:ro" -v "$HOME/mg-db:/data" --user "$(id -u):$(id -g)" ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src
ls ~/mg-db     # graph.redb
```

A `Permission denied ... must be writable` error on `/data/graph.redb` means the directory (or an existing database) is owned by another user: match it with `--user`, or use a named volume. Once a database was created under one `--user`, keep using it; a later run as the image's default user cannot open it.

## Memory, threads and other settings

The container sizes itself like the native binary ([sizing](indexing.md#sizing-threads-memory-disk)): parse threads from the CPUs it can see, the memory budget from free RAM, and a container memory limit is honoured (the budget is sized for the limit, not the host). All `index` flags work; `--memory` can also be given as an environment variable.

```sh
docker run --rm --memory=512m ghcr.io/p47phoenix/memory-graph:main sysinfo                                 # what the container will size from
docker run --rm --cpus=4 -e MEMORY_GRAPH_MEMORY=1G -v "$PWD:/src:ro" -v mg-data:/data \
  ghcr.io/p47phoenix/memory-graph:main index --org acme --repo api /src --jobs 4 --stats
```

On Docker Desktop the memory source reads `sysinfo(2)+cgroup v2` because `/proc/meminfo` is unreadable to non-root there; on a Linux host it reads `/proc/meminfo+cgroup v2`. The total is the same either way; `sysinfo(2)` has no page-cache figure, so its free-memory reading, and the budget derived from it, is somewhat lower.

## Docker Compose

For repeated use, a `compose.yaml` next to the project fixes the mounts and settings once. The `name:` on the volume keeps it the same `mg-data` volume the `docker run` commands above use (without it Compose prefixes the project name and the database is a different one). The file itself is indexed along with the project (one `yaml` file in the summary).

```yaml
services:
  memory-graph:
    image: ghcr.io/p47phoenix/memory-graph:main
    volumes:
      - ./:/src:ro
      - mg-data:/data
    environment:
      MEMORY_GRAPH_MEMORY: "50%"

volumes:
  mg-data:
    name: mg-data
```

```sh
docker compose run --rm memory-graph index --org acme --repo api /src
docker compose run --rm memory-graph search foo --grain class
docker compose run --rm -T memory-graph search foo --json > hits.json   # -T: no TTY when piping or redirecting
docker compose down -v      # also deletes the database volume
```

For a three-node cluster with Compose, see [docs/deploy/compose.md](../deploy/compose.md).

## Shell alias

A one-line wrapper makes the container feel like the native binary (in Git Bash, `export MSYS_NO_PATHCONV=1` first):

```sh
alias mg='docker run --rm -v "$PWD:/src:ro" -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main'
mg index --org acme --repo api /src
mg search foo --grain method
mg search foo --json > hits.json
```

Add `-t` to see the live view while indexing, but not when piping or redirecting `--json` output: a TTY merges stderr into stdout and ends lines with CRLF. The same applies to `docker compose run`, which allocates a TTY by default; pass `-T` there when piping.

## Serving from a container

The image exposes port 7000 and has a `HEALTHCHECK` that asks the server itself (`health --server 127.0.0.1:7000`), so a served container reports `healthy`:

```sh
docker network create mg
docker run -d --name mg-server --network mg -p 127.0.0.1:7000:7000 -v mg-data:/data \
  ghcr.io/p47phoenix/memory-graph:main serve --data-dir /data --bootstrap --node-id 1 --listen 0.0.0.0:7000
docker run --rm --network mg -v "$PWD:/src:ro" ghcr.io/p47phoenix/memory-graph:main \
  --server mg-server:7000 index --org acme --repo api /src
memory-graph --server 127.0.0.1:7000 search foo          # from the host, through the published port
docker stop mg-server                                     # SIGTERM: a graceful stop, /data/LOCK is removed
docker start mg-server                                    # same command again: --bootstrap on an initialized /data is a plain restart
```

A server runs one worker thread per CPU it can see, and on Docker Desktop that is every core of the VM. For fewer threads, less memory and somewhat less idle CPU, give the container fewer CPUs (`docker run --cpus=2`, which also lowers the default) or set the count: `serve --worker-threads 2`, `-e MEMORY_GRAPH_WORKER_THREADS=2`, or tokio's own `-e TOKIO_WORKER_THREADS=2` ([server guide](server.md#serve-options-and-the-lock-file)). Idle workers park, so the idle wakeup rate hardly moves (measured 8.3 to 8.2 per second at `--cpus=2`); it comes from the Raft tick, which a server leading alone (`--db`, or a one-node `--data-dir`) suspends.

See the [server](server.md) and [cluster](cluster.md) guides for what these flags do.

## Idle CPU on macOS (Docker Desktop)

On a Mac, containers run in a Linux VM. Each time a thread in the VM wakes (a timer, a futex), the hypervisor exits to macOS, and that time is billed to the `com.docker.virtualization` process, not to the container. So an idle `serve` can show next to nothing in `docker stats` and still keep the Mac's CPU busy. An idle server now wakes about 3 times a second (`serve --db`) or 5-6 times a second (a one-node `--data-dir`); before issue #205 it was about 40. The [idle CPU spike](../spikes/idle-cpu.md) has the numbers and causes. To keep the cost down:

- **Run the image for your Mac's architecture.** The image is multi-arch (`linux/amd64` and `linux/arm64`), so on Apple silicon Docker pulls the arm64 one. If you forced `--platform linux/amd64`, it runs under Rosetta or QEMU, which costs much more for every wakeup. Check:

  ```sh
  docker image inspect -f '{{.Architecture}}' ghcr.io/p47phoenix/memory-graph:main   # arm64 on Apple silicon
  ```

- **Use a named volume for `/data`, not a bind mount.** A bind mount goes through Docker Desktop's file sharing to macOS; a named volume stays inside the VM.
- **Limit the CPUs.** The server sizes its thread pool from the CPUs it sees. With `--cpus=2` (Compose: `cpus: 2`) the pool is 2 threads instead of one per VM CPU. To size only the pool, use `serve --worker-threads 2` or `-e MEMORY_GRAPH_WORKER_THREADS=2` (see above). A smaller pool means fewer threads and less memory, but hardly fewer wakeups.
- **Run the health check less often.** The image's `HEALTHCHECK` starts a new `memory-graph health` process every 30 s. If nothing watches the status closely, use `docker run --health-interval=5m` (Compose: `healthcheck: interval: 5m`).
- **Keep `MEMORY_GRAPH_LOG` at `info`** (the default). `debug` and `trace` add log lines (openraft's among them), and each line is a write the VM has to wake for.
- **Measure the VM, not only the container.** Look at `com.docker.virtualization` in Activity Monitor (or `top -o cpu`) with the server idle, and compare it with the container stopped. `docker stats` shows only what the container's processes used inside the VM.

To count the wakeups of a running container, `scripts/idle-cpu.sh <container> [seconds] [max]` (in this repository) runs a small alpine container in the server's pid namespace and prints the wakeups/s and CPU per thread and in total. The Docker workflow runs it in CI against an idle `serve --db` and fails above 8 wakeups/s.

```sh
docker run -d --name mg-server --cpus=2 -v mg-data:/data ghcr.io/p47phoenix/memory-graph:main serve --db /data/graph.redb --listen 0.0.0.0:7000
scripts/idle-cpu.sh mg-server 60      # TOTAL cpu=...% of one core, wakeups/s=...
```

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `manifest unknown` on pull or run | No tag given, so Docker asked for `latest`, which does not exist until the first release. Use `:main` or a `sha-…`/version tag. |
| `` `/src` is not a directory`` or a `C:/Program Files/Git/...` path in the error | Git Bash path conversion. Prefix the command with `MSYS_NO_PATHCONV=1`, or use PowerShell. |
| ``database `./graph.redb` does not exist`` on `search`/`describe` | The `/data` mount differs from the one `index` used. Mount the same named volume or directory. |
| `Permission denied ... must be writable` | A bind-mounted `/data` owned by another user. Add `--user "$(id -u):$(id -g)"` or use a named volume. |
| No progress lines, only the summary | Progress needs a terminal: add `-t`. |
| `exec: "/bin/sh": stat /bin/sh: no such file or directory` | The image has no shell by design. Use the `memory-graph` commands. |

## Tags and releases

| Tag | Points at |
|---|---|
| `main` | The latest push to `main` (moves). |
| `sha-<short commit>` | That commit. |
| `0.1.0`, `0.1` | A release tag `v0.1.0`. |
| `latest` | The newest release. Never a prerelease (`v0.2.0-rc1`); absent until the first release. |

A release is cut by pushing a tag that matches the workspace version in `Cargo.toml` (bumped by hand; the workflow refuses a tag that does not match):

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The workflow (`.github/workflows/docker.yml`) builds and smoke-tests the image on every pull request and manual run but only pushes on `main` and `v*` tags.

## Build locally

```sh
docker build -t memory-graph .                                  # host architecture
docker buildx build --platform linux/arm64 --load -t memory-graph .   # cross-compiled; no QEMU needed to build
```
