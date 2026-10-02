#!/bin/sh
# Idle wakeups and CPU of a running memory-graph container (issue #205).
#
#   scripts/idle-cpu.sh <container> [seconds] [max-wakeups-per-s]
#
# The image is FROM scratch (no shell, no tools), so the measurement runs
# in an alpine sidecar that shares the container's pid namespace
# (`docker run --pid=container:<container>`), where the server is pid 1.
# It snapshots every thread's CPU time (/proc/1/task/*/stat, utime+stime)
# and context switches (/proc/1/task/*/status, voluntary+nonvoluntary),
# sleeps for the window (default 60 s), snapshots again, and prints, per
# thread and in total, the CPU used and the wakeups per second. A context
# switch is a wakeup; on Docker Desktop for macOS each one costs a VM exit,
# billed to com.docker.virtualization rather than to the container in
# `docker stats`, so the wakeup rate is the number that matters there. A
# thread that exits within the window is not counted; one born in it
# counts in full.
#
# With a max, exits 1 when the total wakeups/s exceed it (the docker
# workflow runs it against an idle `serve --db`). Exits 2 on bad usage or
# when the container is not running. Needs docker and the alpine image
# (pulled if missing). See docs/spikes/idle-cpu.md.
set -eu

usage() {
    echo "usage: $0 <container> [seconds] [max-wakeups-per-s]" >&2
    exit 2
}

[ $# -ge 1 ] && [ $# -le 3 ] || usage
container=$1
seconds=${2:-60}
max=${3:-}
case $seconds in '' | *[!0-9]* | 0*) usage ;; esac
case $max in *[!0-9.]* | .* | *.*.*) usage ;; esac

if [ "$(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null)" != true ]; then
    echo "$0: container $container is not running" >&2
    exit 2
fi

# The sidecar's program comes on stdin; $1 is the window in seconds.
# /proc/<tid>/stat is split after the ")" closing the thread name (which
# may hold spaces), so utime and stime are fields 12 and 13 of the rest.
# Clock ticks are USER_HZ (100 on Linux): one tick is 10 ms.
out=$(docker run -i --rm --pid="container:$container" alpine:3 sh -s "$seconds" <<'EOF'
snap() {
    for t in /proc/1/task/*; do
        id=${t##*/}
        name=$(tr ' ' '_' < "$t/comm" 2>/dev/null) || continue
        ticks=$(sed 's/.*) //' "$t/stat" 2>/dev/null | awk '{print $12 + $13}') || continue
        sw=$(awk '/^(non)?voluntary_ctxt_switches:/ {s += $2} END {print s + 0}' "$t/status" 2>/dev/null) || continue
        [ -n "$ticks" ] && echo "$id $name $ticks $sw"
    done
}
snap > /tmp/a
sleep "$1"
snap > /tmp/b
echo "threads=$(ls /proc/1/task | wc -l) window=$1s"
awk -v S="$1" '
    NR == FNR { t[$1] = $3; w[$1] = $4; next }
    {
        dt = $3 - t[$1]; dw = $4 - w[$1]
        tot += dt; tw += dw
        printf "%-7s %-22s cpu_ms=%7d wakeups/s=%8.1f\n", $1, $2, dt * 10, dw / S
    }
    END { printf "TOTAL cpu=%.3f%% of one core, wakeups/s=%.1f\n", tot * 10 / (S * 1000) * 100, tw / S }
' /tmp/a /tmp/b > /tmp/r
grep -v '^TOTAL' /tmp/r | sort -t= -k3 -rn
grep '^TOTAL' /tmp/r
EOF
)
echo "$out"

[ -n "$max" ] || exit 0
total=$(echo "$out" | sed -n 's/^TOTAL .*wakeups\/s=//p')
if [ -z "$total" ]; then
    echo "$0: no TOTAL line in the measurement" >&2
    exit 2
fi
if awk -v t="$total" -v m="$max" 'BEGIN { exit !(t > m) }'; then
    echo "$0: $container woke $total times/s while idle, more than $max" >&2
    exit 1
fi
echo "$container: $total wakeups/s while idle, within $max"
