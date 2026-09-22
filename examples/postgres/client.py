"""A CRUD workload against the run's PostgreSQL server, from four threads.

Waits for the server (its host starts at the same virtual moment), then
inserts, updates, deletes and reads back; what it prints is the record
the run must repeat.
"""

import threading
import time

import pg8000.dbapi as pg

THREADS, ROWS, ROUNDS = 4, 100, 20


def connect():
    while True:
        try:
            return pg.connect(host="db", port=5432, user="guest", database="postgres")
        except Exception:
            time.sleep(0.2)


def worker(n, log):
    conn = connect()
    cur = conn.cursor()
    for r in range(ROUNDS):
        lo = n * ROWS
        cur.execute("update items set qty = qty + %s where id > %s and id <= %s", (r, lo, lo + ROWS))
        cur.execute("delete from items where id = %s", (lo + r + 1,))
        cur.execute("insert into items (name, qty) values (%s, %s)", (f"t{n}r{r}", r))
        conn.commit()
        cur.execute("select count(*), coalesce(sum(qty), 0) from items where id > %s and id <= %s", (lo, lo + ROWS))
        log.append((n, r, cur.fetchone()))
    conn.close()


def main():
    conn = connect()
    cur = conn.cursor()
    cur.execute("create table items (id serial primary key, name text, qty int)")
    cur.execute("insert into items (name, qty) select 'item' || g, g from generate_series(1, %s) g", (THREADS * ROWS,))
    conn.commit()
    log = []
    threads = [threading.Thread(target=worker, args=(n, log)) for n in range(THREADS)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for n, r, row in log:
        print(f"thread {n} round {r}: {row[0]} rows, qty {row[1]}")
    cur.execute("select count(*), sum(qty), max(id) from items")
    print("final:", cur.fetchone())
    cur.execute("select name from items order by id desc limit 5")
    print("last:", [r[0] for r in cur.fetchall()])
    conn.close()


main()
