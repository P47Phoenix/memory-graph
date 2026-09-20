#!/bin/sh
# S1: in-process vs Unix-socket round trip. usage: run_s1.sh <workdir> <label> <db>
# Three reflink copies are used because redb locks the file: one for the in-process run, one per server.
set -e
W=$1; L=$2; DB=$3
D=$(dirname "$0")/target/release/daemon-spike
for k in inp json bin; do cp --reflink=auto "$DB" "$W/$L.$k.redb"; done
rm -f "$W/$L.server.log"
"$D" serve "$W/$L.json.redb" "$W/$L.json.sock" json 2>>"$W/$L.server.log" & P1=$!
"$D" serve "$W/$L.bin.redb" "$W/$L.bin.sock" bin 2>>"$W/$L.server.log" & P2=$!
sleep 15   # wait for the servers to open the store
"$D" bench "$W/$L.inp.redb" "$L" "$W/$L.json.sock" "$W/$L.bin.sock"
# the server prints its per-request handle() times when the client disconnects (bench has exited by now)
sleep 2; kill $P1 $P2; sleep 1
echo "--- server-side handle() time (same store, no socket/codec) ---"
grep server_handle "$W/$L.server.log" | sed 's/req=//'
rm -f "$W/$L".*.redb "$W/$L".*.sock
