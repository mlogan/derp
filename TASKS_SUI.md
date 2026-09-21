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
`the_outside_network_is_refused_unless_allowed` (`outside.c`). The far
callee tails and unreachable sites have no test: they need a program of
over 120 MB.

## Not done

- A run file for a multi-process network (`sui genesis` then one
  `sui-node` per validator on its own virtual host): the config's
  absolute paths would need rewriting to the hosts' directories.
- The trace `site` of a quantum that ended without a switch is not in the
  schedule hash; `repeat` would not see the residual above.
