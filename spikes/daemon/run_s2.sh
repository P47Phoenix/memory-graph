#!/bin/bash
# S2: two-process lock behaviour. usage: run_s2.sh <workdir> <repo-root>
W=$1; R=$2
D=$R/spikes/daemon/target/release/daemon-spike
M=$R/target/release/memory-graph
ms() { echo "scale=1; ($2-$1)/1000000" | bc; }

if [[ -z $ONLY || $ONLY == *1* ]]; then
echo "== 1. lock hold time of one short process (open + search + close), by DB size =="
for f in d1x d1m d10m; do cp --reflink=auto $W/$f.redb $W/s2_$f.redb; echo "-- $f"; for i in 1 2 3 4 5; do $D search-once $W/s2_$f.redb; done; rm -f $W/s2_$f.redb; done

fi
if [[ -z $ONLY || $ONLY == *2* ]]; then
echo; echo "== 2. what a second process sees =="
cp --reflink=auto $W/d1x.redb $W/s2.redb
$D hold $W/s2.redb 3 > /dev/null & HP=$!; sleep 0.5
echo "-- library:"; $D try $W/s2.redb
echo "-- CLI search:"; s=$(date +%s%N); $M --db $W/s2.redb search self --limit 3; echo "exit=$? in $(ms $s $(date +%s%N)) ms"
echo "-- CLI index-file:"; echo 'fn a(){}' > $W/x.rs; s=$(date +%s%N); $M --db $W/s2.redb index-file --org o --repo r $W/x.rs; echo "exit=$? in $(ms $s $(date +%s%N)) ms"
wait $HP

fi
if [[ -z $ONLY || $ONLY == *3* ]]; then
echo; echo "== 3. real long index run: hold time + what a retrying reader sees =="
rm -f $W/s2_big.redb
s=$(date +%s%N); $M --db $W/s2_big.redb index --org big --repo big $W/bigsrc > $W/s2_big_index.out 2>&1 & IP=$!
sleep 1
$D retry $W/s2_big.redb 5 250 300 jitter > $W/s2_retry_big.out; RET_NS=$(sed -n 's/.*retry_ok_ns=\([0-9]*\).*/\1/p' $W/s2_retry_big.out)
wait $IP; e=$(date +%s%N)
echo "index process wall (= lock hold): $(ms $s $e) ms; $(grep 'indexed' $W/s2_big_index.out | tail -1)"
echo "reader (started 1 s in, jittered backoff base 5 ms cap 250 ms): $(cat $W/s2_retry_big.out); success $(ms $RET_NS $e) ms after the indexer exited (negative = before, impossible)"

fi
if [[ -z $ONLY || $ONLY == *4* ]]; then
echo; echo "== 4. retry policy vs holder duration (holder = open then sleep; overshoot = success - release) =="
echo "holder_s  policy                      trial  attempts  waited_ms  overshoot_ms"
for H in 0.25 1 5 30; do
  case $H in 0.25|1) N=10;; 5) N=6;; 30) N=2;; esac
  for pol in "5 250 120 jitter" "100 100 120 fixed" "20 1000 120 jitter"; do
    for i in $(seq 1 $N); do
      $D hold $W/s2.redb $H > $W/s2_hold.out & HP=$!
      sleep 0.05
      $D retry $W/s2.redb $pol > $W/s2_r.out 2>&1 || true
      wait $HP
      REL=$(sed -n 's/held_release_ns=//p' $W/s2_hold.out); OK=$(sed -n 's/.*retry_ok_ns=\([0-9]*\).*/\1/p' $W/s2_r.out)
      A=$(sed -n 's/.*attempts=\([0-9]*\).*/\1/p' $W/s2_r.out); WT=$(sed -n 's/.*waited_ms=\([0-9.]*\).*/\1/p' $W/s2_r.out)
      echo "$H  $pol  $i  $A  $WT  $(ms $REL $OK)"
    done
  done
done

fi
if [[ -z $ONLY || $ONLY == *5* ]]; then
echo; echo "== 5. contention: 8 short-lived reader processes x 100 open+search+close, no long holder =="
for i in 1 2 3 4 5 6 7 8; do $D storm-worker $W/s2.redb 100 2 100 & done; wait
fi
if [[ -z $ONLY || $ONLY == *6* ]]; then
echo; echo "== 6. same 8 readers while one indexer holds the DB (started 1 s earlier, ~real index of bigsrc) =="
cp --reflink=auto $W/d1x.redb $W/s2b.redb
s=$(date +%s%N); $M --db $W/s2b.redb index --org big --repo big2 $W/bigsrc --reindex > /dev/null 2>&1 & IP=$!
sleep 0.5
for i in 1 2 3 4 5 6 7 8; do $D storm-worker $W/s2b.redb 20 2 100 & done
wait; echo "(indexer + readers finished; total wall $(ms $s $(date +%s%N)) ms)"
rm -f $W/s2*.redb $W/x.rs
fi
