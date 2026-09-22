"""A CRUD workload against the run's Redis server, from four threads.

Waits for the server (its host starts at the same virtual moment), then
sets, increments, pushes, pops and deletes; what it prints is the record
the run must repeat.
"""

import threading
import time

import redis

THREADS, KEYS, ROUNDS = 4, 50, 20


def connect():
    while True:
        try:
            r = redis.Redis(host="cache", port=6379, socket_connect_timeout=1)
            r.ping()
            return r
        except redis.exceptions.RedisError:
            time.sleep(0.2)


def worker(n, log):
    r = connect()
    for i in range(ROUNDS):
        pipe = r.pipeline()
        for k in range(KEYS):
            pipe.incrby(f"counter:{k}", n + 1)
        pipe.hset(f"thread:{n}", f"round{i}", i * n)
        pipe.lpush("queue", f"t{n}r{i}")
        pipe.execute()
        item = r.rpop("queue")
        if i % 5 == 4:
            r.delete(f"counter:{(n * ROUNDS + i) % KEYS}")
        log.append((n, i, item, r.llen("queue")))


def main():
    r = connect()
    r.flushall()
    log = []
    threads = [threading.Thread(target=worker, args=(n, log)) for n in range(THREADS)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for n, i, item, qlen in log:
        print(f"thread {n} round {i}: popped {item}, queue {qlen}")
    total = sum(int(r.get(f"counter:{k}") or 0) for k in range(KEYS))
    print("counters:", r.dbsize() - THREADS - (1 if r.exists("queue") else 0), "total", total)
    print("hashes:", [r.hlen(f"thread:{n}") for n in range(THREADS)])
    print("queue:", r.lrange("queue", 0, -1))


main()
