# Running a cluster on Kubernetes

**TL;DR.** `kubectl apply -f deploy/kubernetes/` creates a three-member cluster: a StatefulSet
`memory-graph` (one PersistentVolumeClaim per pod), a headless Service that gives every pod a
stable DNS name, a client Service over the ready pods, and a PodDisruptionBudget of
`minAvailable: 2`. Pod `memory-graph-N` is node N+1; pod 0 bootstraps the cluster and the others
join it. Readiness is the gRPC health service `memory-graph.ready`. Before scaling down, remove
the member with `cluster remove`. Design: ADR 0004 D10, epic story 24. The manifests are checked
by `crates/graph-cli/tests/deploy_manifests.rs` (YAML parse, flags against `serve --help`); no
cluster runs in CI.

## The manifests

| File | Object | Why |
|---|---|---|
| [`service.yaml`](../../deploy/kubernetes/service.yaml) | Service `memory-graph` (headless, `clusterIP: None`, `publishNotReadyAddresses: true`) | Stable per-pod DNS: `memory-graph-N.memory-graph.<ns>.svc.cluster.local`. Not-ready pods are published because a joining or lagging pod must still be reachable by its peers. |
| | Service `memory-graph-client` | One virtual IP over the ready pods, for clients (`--server memory-graph-client:7000`). |
| [`statefulset.yaml`](../../deploy/kubernetes/statefulset.yaml) | StatefulSet `memory-graph`, 3 replicas, `volumeClaimTemplates` `data` (20Gi) at `/data` | One data directory per pod that survives rescheduling. |
| [`pdb.yaml`](../../deploy/kubernetes/pdb.yaml) | PodDisruptionBudget `minAvailable: 2` | A drain or upgrade takes at most one of three members; two keep a quorum, so writes continue. |

## Identity without a shell

The image is `scratch` (no shell for an entrypoint script), so `serve` derives everything from the
pod itself:

```yaml
env:
  - name: POD_NAME
    valueFrom: { fieldRef: { fieldPath: metadata.name } }
  - name: POD_NAMESPACE
    valueFrom: { fieldRef: { fieldPath: metadata.namespace } }
args:
  - serve
  - --data-dir
  - /data
  - --listen
  - 0.0.0.0:7000
  - --advertise
  - $(POD_NAME).memory-graph.$(POD_NAMESPACE).svc.cluster.local:7000
  - --node-id-from-hostname
  - --bootstrap-or-join
  - memory-graph-0.memory-graph.$(POD_NAMESPACE).svc.cluster.local:7000
  - --auto-promote
```

- **`--node-id-from-hostname`**: the node id is the trailing `-<ordinal>` of the host name plus
  one (`memory-graph-0` is node 1, `memory-graph-2` is node 3). The host name is read from
  `HOSTNAME` (a pod's is its name), then `/etc/hostname`; a name without an ordinal is refused.
- **`--bootstrap-or-join <peer>`**: ordinal 0 behaves as `--bootstrap`, every other ordinal as
  `--join <peer>`. With `--auto-promote`, a joined pod becomes a voter once it has caught up.
- **`--advertise`** uses the headless Service name, which Kubernetes expands from the env vars
  (`$(VAR)` in `args`). It is recorded in `node.json` and the membership, so it must stay the same
  across restarts (it does: the pod name and namespace are stable).
- **Restarts** find an initialized `/data` and resume: `--bootstrap` and `--join` on a data
  directory that belongs to the cluster are plain restarts.
- **`podManagementPolicy: Parallel`**: after a full stop, no single pod can become ready alone (a
  leader needs two of three voters), so the default ordered start-up would wait for pod 0 forever.
  In parallel, pods 1 and 2 may start before pod 0 answers; a first `--join` retries for
  `--join-timeout` (default 2m) and the container restarts if it runs out.

## Probes

```yaml
startupProbe:   { grpc: { port: 7000 }, periodSeconds: 2, failureThreshold: 150 }
readinessProbe: { grpc: { port: 7000, service: memory-graph.ready }, periodSeconds: 5 }
livenessProbe:  { grpc: { port: 7000 }, periodSeconds: 10, failureThreshold: 6 }
```

Kubernetes 1.27+ probes gRPC health natively (no exec, which a scratch image could not run
anyway except through `/memory-graph health`).

- **Liveness** asks the default service (`""`): `SERVING` once the store is open. A pod without a
  leader (a partition, a lost quorum) stays alive: restarting it would not help.
- **Readiness** asks `memory-graph.ready`: `SERVING` only while a leader is known **and** the pod
  has applied to within `--ready-max-lag` entries (default 1000) of the leader's commit index. A
  learner still catching up, or a pod cut off from the leader, is taken out of
  `memory-graph-client`. The same check from a shell: `memory-graph health --ready --server
  <pod>:7000` (exit 0 ready, 1 otherwise).
- During shutdown both services go `NOT_SERVING` first, then in-flight requests finish
  (`terminationGracePeriodSeconds: 45` covers the server's 30 s drain).

## Metrics and logs

Every pod serves Prometheus text on `:9100/metrics` (`--metrics-listen`); the pod template has the
common `prometheus.io/*` annotations. For the Prometheus Operator, a PodMonitor selecting
`app.kubernetes.io/name: memory-graph` on port `metrics` does the same. Logs are JSON lines on
stderr (`--log-format json`), level from `MEMORY_GRAPH_LOG`. The metric names are listed in the
[README](../../README.md#observability).

## Scaling

**Up** (3 to 5): `kubectl scale statefulset memory-graph --replicas 5`. Pods 3 and 4 join with
`--auto-promote` and become voters once caught up. Raise the PodDisruptionBudget to
`minAvailable: 3` (a majority of five).

**Down** (5 to 3), one pod at a time, highest ordinal first:

```sh
kubectl exec memory-graph-0 -- /memory-graph --server 127.0.0.1:7000 cluster remove 5   # pod memory-graph-4 is node 5
kubectl scale statefulset memory-graph --replicas 4
kubectl delete pvc data-memory-graph-4                                                  # its data is no longer a member's
# then node 4 / pod 3 the same way
```

Remove the member **before** reducing replicas: a voter that disappears without `cluster remove`
still counts toward the quorum, so the cluster tolerates one fewer failure until it is removed
(and at three voters, losing two more stops writes). `cluster remove` refuses the leader (transfer
it first with `cluster transfer-leader`), any removal that leaves fewer reachable voters than a
quorum, and 3 voters down to 2 without `--force`. Delete the pod's claim afterwards: a later
scale-up with the old claim would restart a removed node instead of joining it again.

## Caveats

- **Pod 0 losing its volume** would bootstrap a *new* cluster (it sees an empty directory and
  `--bootstrap-or-join` says bootstrap), which the other pods refuse (`WrongCluster`). To replace
  pod 0's volume: `cluster remove 1` from another pod (after a `transfer-leader` if it led), delete
  the claim, and start pod 0 once with its args changed to `--join
  memory-graph-1.memory-graph.<ns>.svc.cluster.local:7000 --auto-promote` (a StatefulSet patch),
  then restore the args.
- **No TLS or authentication** yet (a later stage of ADR 0004): keep the Services cluster-internal
  and restrict them with a NetworkPolicy.
- Backup and restore: [data-dir.md](data-dir.md). Compose: [compose.md](compose.md).
