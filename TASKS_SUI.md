# Sui under the supervisor: progress

Goal: run `sui start` (a local network: a validator and a fullnode in one
process) deterministically under `rewrite run`, end the run after a fixed
virtual time, and compare `RUST_LOG=trace` output between runs.

Branch `mlogan-sui`. Sui checkout: `~/repos/sui` (main, 1.74.0), built
with `cargo build --release --bin sui` (the `release` profile:
`panic=abort`, line tables only, a dSYM). The debug build cannot be
rewritten: its code is 291 MB, past any direct branch's reach.

## Result (2026-09-22)

`sui start --force-regenesis` for 30 virtual seconds (about 3 s real):
genesis, a validator, a fullnode, 24 checkpoints executed. Three runs of
seed 1 give the same schedule hash, the same 12,500 switches, and
byte-identical logs; the schedule traces are identical too, except that
on some runs one line differs (below). With `RUST_LOG=trace` two runs
give identical 17,544-line logs and identical traces.

Run file (`stop-after` ends it; the JWK fetch reaches the internet, so
`outside-network` stays refused, which fails it at once):

```yaml
seed: 1
stop-after: 30s
env: { RUST_LOG: info, RUST_LOG_FILE: sui.log }
hosts:
  - name: net
    processes:
      - argv: [/path/to/sui, start, --force-regenesis]
```

`rewrite run --capture --capture-stderr --scratch DIR --manifest FILE`;
the log is `DIR/net/sui.log.2027-01-15` (the virtual clock's date).

## What had to change (in commit order)

1. **`stop-after`** (`--stop-after 30s`, `stop-after:` in the run file):
   the scheduler crashes every live process when the virtual clock
   reaches the time, marks them stopped, restarts nothing after it, and
   the baton holder writes its report before it dies. Stopped processes
   report `stopped` and do not fail the run. `--capture-stderr`.
2. **Compact stubs**: one shared body per program, a 4 to 6 word
   trampoline per site (18 bytes per site instead of 64), the site named
   by the trampoline's return address on the stack. sui's 2.5 million
   sites fit in 45 MB.
3. **Reach**: a callee too far for a `b` goes through x16 (a call
   clobbers it by ABI); sites that cannot reach the segment (the first
   2 MB of sui's text: `__LINKEDIT` starts 136 MB past `__TEXT`) are left
   alone and counted, 2.5% of them. Placing stubs below `__TEXT` was
   tried three ways and is impossible: dyld refuses segments out of
   address or file order, and the kernel wants `__PAGEZERO` to cover the
   low 4 GB and extends the reservation to the first slid segment. (An
   LTO build, `--profile release-lto`, has 85 MB of code and 2 unreachable
   sites, if full coverage matters.)
4. **ulock waits answer libdispatch's way**: `ULF_NO_ERRNO` callers get
   negative errno values; -1 read as `-EPERM` was "BUG IN LIBDISPATCH:
   ulock_wait() failed" inside the Security framework. A wait that times
   out on the virtual clock in a process with threads outside the schedule
   waits the same again for real.
5. **System libraries' waits run in the kernel with the baton**: a wait
   libdispatch or libxpc makes for itself is ended by a GCD worker at a
   moment of real time; in the scheduler that was 14 outside wakes per
   run and the first divergence. Shims give the interposers the caller's
   return address; the shared cache tells a system library's call from
   the guest's own. `system_wait` in the report.
6. **The outside network is refused** in run-file runs (connections
   `ENETUNREACH`, unknown names `EAI_NONAME`) unless `outside-network:
   allow`: the resolver answered in real time and the JWK fetch reached
   the internet.
7. **Function-table entries without a symbol are not hooked**: blst's
   SHA-256 round constants sit in the text as an unnamed atom, and one of
   them decodes as a backward `b`. Hooked, every SHA-256 was wrong, and a
   genesis made natively failed its proof-of-possession check.
8. **Scheduled threads' mappings go to a reserved region** in schedule
   order (`mach_vm_map`, `mach_vm_allocate`, `mmap`): the kernel placed
   thread stacks first-fit around GCD workers' stacks, `pthread_self` is a
   stack address, and RocksDB seeds its skip-list heights from a hash of
   it; runs parted in `MemTable::Add`. `mappings_placed` in the report.
9. **`pthread_threadid_np`** of a scheduled thread is its index in the run
   (kernel thread ids are system-wide).
10. **An `mmap` with an address hint is placed too**, not passed to the
    kernel. jemalloc hints at the end of its last extent, which lies in
    the region; the kernel grants the hint when the range is free, and
    the stack of a scheduled thread that has exited is free once the
    kernel has got round to freeing it (in the terminate syscall, real
    time, no user-space call to see). Runs with jemalloc parted in its
    radix tree (`rtree_metadata_read`, `lg_ceil`) on one run in three.
    `mappings_hinted` in the report.

## Build sizes and reach (2026-09-22)

| build | code (`__text`) | `__TEXT` | sites hooked | unreachable |
|---|---|---|---|---|
| `sui` release | 106 MB | 131 MB | 2.47 M | 65 k (2.5%) |
| `sui` release-lto (21 min) | 85 MB | 99 MB | 2.04 M | 2 |
| `sui` debug | 291 MB | 400 MB | 368 k | 7.5 M (95%) |
| `sui-node` debug | 180 MB | 246 MB | 1.46 M | 3.4 M (70%) |
| `sui-node` `--profile simulator` (opt-level 1, debug assertions, 5 min) | 79 MB | 113 MB | 2.07 M | 10.6 k (0.5%) |

The debug `sui` still runs and repeats (two runs of `sui start`: same
hash, same logs), but with hooks in the last 12 MB of its code only:
almost no preemption, so not a build to find races with. `sui-node` at
opt-level 1 is the practical debug target. No arm64 instruction reaches
further than a `b` in one word; for true `-O0` builds the way is room
inside the text at link time (generated `.space` objects placed by an
`-order_file` every 100 MB), which the rewriter would fill with
trampolines. Not built.

## Rooms: the `-O0` sui-node (2026-09-22)

`rewrite cargo build -p sui-node --bin sui-node --no-default-features`
from the sui checkout. The wrapper links `sui-node` twice: the second
time with two rooms (80 MB and 48 MB of `.space`, placed at 48 MB and
230 MB of the text by a 775k-line order file); the file grows from 491 to
619 MB and its text from 180 to 305 MB. `rewrite scan`: 4.79 million
sites hooked, 0 unreachable, 75 MB of trampolines in the rooms. The
node (one validator, `sui genesis` config, `stop-after: 120s`, about 4 s
real) runs consensus and executes checkpoints; three runs give the same
hash, 535,232,343 hooks, identical traces and logs.

sui-node's default allocator is jemalloc, which keeps the heap out of
the seeded allocator; two runs with it parted inside jemalloc's own code
(`rtree_metadata_read`, `lg_ceil`) with identical logs but different
hashes. The cause was change 10 above (a hinted `mmap` granted or not by
a real-time race with an exiting thread's stack); with it, three runs
with jemalloc agree (hash 4dc53a27de14494b, 539,458,903 hooks, 9 hinted
mappings), and nine runs without the fix agreed too: the race is rare.
`--no-default-features` is still the way to a seeded heap layout.

The planner had two bugs on this binary that the 150 MB test program did
not show: a stretch boundary past the end of the text made its loop push
rooms forever (26 GB, then the kernel killed the linker), and the
room-size iteration oscillated between two answers; the size is now the
closed-form fixed point of the local site density (0.7 bytes of
trampoline per byte of `-O0` text: a room per 104 MB).

## Known residual

On some runs one trace line differs: the quantum expires at a different
instruction of `rocksdb::DecodeSessionId`, with the same hook count and
clock, and nothing after it changes (the schedule hash covers switches,
not where a thread kept the baton, and the logs agree). The DB session id
comes from an entropy struct RocksDB hashes whole, which holds a 64-byte
hostname buffer of which only the name is written; the rest is whatever
the stack held. Not chased further: identifiers only.

## What is input, still

- The Security framework's certificate loading (any reqwest client:
  `rustls-native-certs`) runs on GCD workers: their work is done while the
  guest waits in the kernel, and their allocations go to libc, but what
  they compute is input. 63 such waits per run.
- `sendmsg` ancillary data (quinn's ECN and packet info) is dropped on
  virtual sockets, with a log line per call: noisy, harmless so far.
- `setsockopt` options 0x7, 0x1b, 0x1c at level 0 (IP_TOS, IP_RECVTOS,
  IP_RECVPKTINFO) are ignored.
- The guest looks at `/Users/.../target/release` and the framework
  snapshot directory, refused by the host-directory policy; it copes.

## Tests added

`a_run_is_over_at_its_stop_time`, `a_process_crashed_before_the_stop_is_
not_restarted_after_it`, `a_single_program_run_stops_too`,
`an_unnamed_constant_table_in_the_text_is_left_alone` (`asm_table.c`),
`thread_stacks_and_mappings_land_in_the_region_and_repeat` (`stacks.c`),
`a_hinted_mapping_is_placed_by_the_run_not_granted_by_the_kernel`
(`hint.c`),
`the_outside_network_is_refused_unless_allowed` (`outside.c`). The far
callee tails and unreachable sites have no test: they need a program of
over 120 MB.

## Not done

- A run file for a multi-process network (`sui genesis` then one
  `sui-node` per validator on its own virtual host): the config's
  absolute paths would need rewriting to the hosts' directories.
- The trace `site` of a quantum that ended without a switch is not in the
  schedule hash; `repeat` would not see the residual above.
