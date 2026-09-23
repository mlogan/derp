# A Sui cluster under the supervisor: progress

Goal: run sui-operations' Antithesis cluster
(`docker/sui-antithesis/docker-compose-antithesis.yaml`) as a run file:
four validators, a fullnode, an observer fullnode and the `stress`
client, each on its own host, repeatably. Branch `mlogan-sui-cluster`,
sui checkout `~/repos/sui` (main, 1.74.0, release build).

## Result (2026-09-23)

`examples/sui/run.sh` builds the binaries with `rewrite cargo`, makes the
genesis natively once and runs `examples/sui/run.yaml`. Stress's first
life fails to reach the fullnode and is restarted; the second runs its
minute of workload (12 tps of the 15 asked, 0% errors, p50 624 ms) and
the run ends at about 151 virtual seconds, 4 minutes real. Three full
runs of seed 1 are identical (`rewrite repeat --runs 3`, 13 minutes),
as are eight runs stopped at 2 s.

## What had to change

1. **The gas key's address.** `sui keytool --keystore-path X list`
   listed a key from `~/.sui`, not X; the address in the genesis was
   that key's. The committed `sui.keystore` belongs to `0x9b1b…3481`.
2. **Long quanta.** At the default quantum each 1000..10000 hooks cost a
   millisecond of virtual time: four validators in one `sui start`
   executed 38 checkpoints in 200 virtual seconds, stalled on a
   settlement transaction's effects, against 438 in 100 s natively.
   `quantum: 100000..1000000` gave 758, no stalls.
3. **`switch-cost`.** With long quanta alone the fullnodes ran at half
   the validators' pace (checkpoint 450 against 1050) with a sixth of
   their hooks: waiting, not computing. Every hand-off cost 1 ms and
   state sync is a chain of round trips; stress's transactions timed
   out waiting for checkpoints. `switch-cost: 50us` (new; default 1 ms)
   keeps every node together.
4. **Relative `allow:` entries.** stress builds Move packages from the
   checkout at paths compiled in; `allow: [sui-src]` with run.sh's link
   to the checkout lets it (relative entries are resolved next to the
   run file, through symlinks).
5. **The system allocator's clock reads.** Runs parted within two
   virtual seconds on five in six, a thread's clock one or two
   microseconds apart at the same hook count. Logging each clock read's
   callers showed `_xzm_free` in libsystem_malloc, under CoreFoundation's
   plist parser (certificate loading): how often `free` reads the time
   depends on heap state GCD workers shape in real time. Such reads now
   see the clock without moving it (README, "Nondeterminism found and
   fixed").

## Known noise

- A "sendmsg: ancillary data is not carried" line per QUIC send.
- Refused opens of `/proc/sys/kernel/random/uuid` and, for backtraces,
  of the rewritten binary's own path; the JWK fetches fail (outside
  network refused). The nodes cope.

## Not done

- Other seeds, fault injection (crashing validators) and net latency on
  this cluster.
- The compose file runs two sui-node versions (upgrade tests); here
  every node runs the same binary.
