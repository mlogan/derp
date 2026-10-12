"""Writes to the run's Redis server while it is stopped and restarted.

One key at a time, retrying a write until it is acknowledged; a refused or
dropped connection means the server is down between lives. At the end it
checks that every acknowledged write is still there: a server that saves
on SIGTERM keeps them all, one that is killed loses what came after its
last save.
"""

import time

import redis

SECONDS = 6


def connect():
    while True:
        try:
            r = redis.Redis(host="cache", port=6379, socket_connect_timeout=1, socket_timeout=1)
            r.ping()
            return r
        except redis.exceptions.RedisError:
            time.sleep(0.05)


def main():
    r = connect()
    r.flushall()
    start = time.monotonic()
    acked, i = 0, 0
    while time.monotonic() - start < SECONDS:
        while True:
            try:
                r.set(f"key:{i}", i)
                break
            except redis.exceptions.RedisError:
                r = connect()
        acked = i + 1
        i += 1
        time.sleep(0.005)
    missing = sum(1 for k in range(acked) if r.get(f"key:{k}") is None)
    print(f"missing: {missing} of {acked}")


main()
