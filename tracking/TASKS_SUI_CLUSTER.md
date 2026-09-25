# A Sui cluster under the supervisor: progress

Goal: run sui-operations' Antithesis cluster
(`docker/sui-antithesis/docker-compose-antithesis.yaml`) as a run file:
four validators, a fullnode, an observer fullnode and the `stress`
client, each on its own host, repeatably. Branch `mlogan-sui-cluster`,
sui checkout `~/repos/sui` (main, 1.74.0, release build).

## Result (2026-09-24)

The sui repository's `scripts/derp/run.sh` (moved there from
`examples/sui`: too special for an example here) builds the binaries
(release) with `derp cargo`, makes the genesis natively once and runs
`scripts/derp/run.yaml`, which
sets no timing: the defaults (quantum 1000..10000, 10 µs per hand-off).
Stress's first life fails to reach the fullnode and is restarted; the
second runs its minute of workload (11 tps of the 15 asked, 0% errors,
p50 about 460 ms) and the run ends at about 143 virtual seconds.

- Ten full runs of seed 1 are identical (`derp repeat --runs 10`,
  56 minutes).
- 60 virtual seconds take 2 min 32 s of wall time (94 s user, 28 s
  system CPU); every node reaches checkpoint 265.
- Tidehunter (`TIDEHUNTER=1 run.sh`, its own target directory, 9.5 min
  to build): consensus, checkpoints and the authority tables report
  tidehunter; 60 virtual seconds take 3 min 27 s and reach checkpoint
  237; three runs are identical. At sui's release defaults a
  tidehunter node allocates a bloom filter (32,000 items, about 57 KB)
  and a value cache per cell up front, with up to 32k cells a keyspace:
  the cluster's peaks summed to 37.6 GB, fullnodes 5.8 GB each. With
  `TH_DEFAULT_MUTEX_COUNT: 16` (sui's debug-build value) and
  `TH_DEFAULT_VALUE_CACHE_SIZE: 100` in the run file, 18.5 GB, the same
  as RocksDB, and 60 virtual seconds take 191 s.
- Memory: after 60 virtual seconds validators hold about 3.3 GB each
  (validator1, which the observer follows, 6 GB), fullnodes 0.85 GB,
  and they grow with virtual time. Not RocksDB: shrinking write
  buffers, WAL and block caches through sui's environment overrides
  changed nothing. Two clusters fit on a 40 GB machine: in parallel
  they take 181 s for 60 virtual seconds each against 157 s alone.
  Runs compare only with scratch paths of one length (the length moves
  every guest's stack).

## What had to change

1. **The gas key's address.** `sui keytool --keystore-path X list`
   listed a key from `~/.sui`, not X; the address in the genesis was
   that key's. The committed `sui.keystore` belongs to `0x9b1b…3481`.
2. **Cheap hand-offs.** Every hand-off, quantum expiries included, cost
   a millisecond of virtual time, and all threads share one clock: four
   validators in one `sui start` executed 38 checkpoints in 200 virtual
   seconds, stalled on a settlement transaction's effects, against 438
   in 100 s natively. Longer quanta helped the validators, but the
   fullnodes, waiting on chains of round trips, ran at half their pace
   and stress's transactions timed out. A hand-off now costs 10 µs by
   default (`switch-cost` changes it); the run file sets no timing and
   every node keeps together. Code timing its own work finds it very
   fast: this is not a simulation of real-time bounds.
3. **Relative `allow:` entries.** stress builds Move packages from the
   checkout at paths compiled in; `allow: [../..]`, the checkout the run
   file sits in, lets it (relative entries are resolved next to the run
   file, through symlinks).
4. **The system allocator's clock reads.** Runs parted within two
   virtual seconds on five in six, a thread's clock one or two
   microseconds apart at the same hook count. Logging each clock read's
   callers showed `_xzm_free` in libsystem_malloc, under CoreFoundation's
   plist parser (certificate loading): how often `free` reads the time
   depends on heap state GCD workers shape in real time. Such reads now
   see the clock without moving it (README, "Nondeterminism found and
   fixed").
5. **RocksDB's ids from the Mach calendar clock.** With cheap
   hand-offs, one run in two parted once in two million switches: a
   fullnode's quantum ended in RocksDB's `LRUCacheShard::ApplyToSomeEntries`
   at another entry, the same hook count. DB and session ids differed
   on every run: `NowNanos` on macOS reads `clock_get_time` on the
   calendar clock service, which went to the kernel. Both Mach calls are
   interposed now (README, "Nondeterminism found and fixed"). The
   residual `TASKS_SUI.md` put down to a hostname buffer was this: the
   entropy struct is zeroed first.

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
