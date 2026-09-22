"""A workload against the run's memcached from four threads: sets,
increments, appends, deletes and a final tally. The server runs four
worker threads of its own; what is printed is the record the run must
repeat.
"""

import threading
import time

from pymemcache.client.base import Client
from pymemcache.exceptions import MemcacheError

THREADS, KEYS, ROUNDS = 4, 40, 25


def connect():
    while True:
        try:
            c = Client(("cache", 11211), connect_timeout=1, timeout=5)
            c.version()
            return c
        except (OSError, MemcacheError):
            time.sleep(0.2)


def worker(n, log):
    c = connect()
    for r in range(ROUNDS):
        c.set(f"w{n}:r{r}", f"round {r}", noreply=False)
        for k in range(KEYS):
            c.incr(f"counter:{k}", n + 1)
        c.append(f"log:{n}", f"{r},", noreply=False)
        if r % 5 == 4:
            c.delete(f"counter:{(n * ROUNDS + r) % KEYS}", noreply=False)
        got = c.get_many([f"w{m}:r{r}" for m in range(THREADS)])
        log.append((n, r, sorted(got)))
        time.sleep(0.001)


def main():
    c = connect()
    c.flush_all()
    for k in range(KEYS):
        c.set(f"counter:{k}", 0)
    for n in range(THREADS):
        c.set(f"log:{n}", "")
    log = []
    threads = [threading.Thread(target=worker, args=(n, log)) for n in range(THREADS)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for n, r, seen in log:
        print(f"thread {n} round {r}: saw {seen}")
    counters = c.get_many([f"counter:{k}" for k in range(KEYS)])
    print("counters:", len(counters), "total", sum(int(v) for v in counters.values()))
    print("logs:", [len(c.get(f"log:{n}").split(b",")) - 1 for n in range(THREADS)])
    stats = c.stats()
    print("stats: curr_items", stats[b"curr_items"], "cmd_get", stats[b"cmd_get"], "cmd_set", stats[b"cmd_set"])


main()
