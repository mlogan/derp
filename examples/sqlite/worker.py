"""One of several processes sharing a SQLite database in WAL mode.

Each worker inserts rows and bumps counters in transactions, retrying
when another writer holds the lock, and prints what it saw at the end.
The order in which the workers get the lock is the schedule's.
"""

import sqlite3
import sys
import time

n = int(sys.argv[1])
ROUNDS, ROWS = 25, 8

db = sqlite3.connect("shared.db", timeout=30, isolation_level=None)
db.execute("pragma journal_mode=wal")
db.execute("pragma synchronous=normal")
db.execute("create table if not exists items (id integer primary key, worker int, round int, note text)")
db.execute("create table if not exists counters (name text primary key, value int)")
db.execute("insert or ignore into counters values ('total', 0)")

seen = []
for r in range(ROUNDS):
    while True:
        try:
            db.execute("begin immediate")
            for i in range(ROWS):
                db.execute("insert into items (worker, round, note) values (?, ?, ?)", (n, r, f"w{n}r{r}i{i}"))
            db.execute("update counters set value = value + ? where name = 'total'", (ROWS,))
            total = db.execute("select value from counters where name = 'total'").fetchone()[0]
            db.execute("commit")
            break
        except sqlite3.OperationalError as e:
            db.execute("rollback") if db.in_transaction else None
            time.sleep(0.01)
    seen.append(total)
    # A pause between rounds, as a client pacing itself: the interpreter
    # runs unhooked, so this is where the others get their turn
    time.sleep(0.001)
    if r % 5 == 4:
        # A read outside any write transaction, while others write
        rows, mine = db.execute("select count(*), sum(worker = ?) from items", (n,)).fetchone()
        seen.append((rows, mine))

print(f"worker {n}: totals seen {seen[:6]} ... last {seen[-1]}")
count, workers = db.execute("select count(*), count(distinct worker) from items").fetchone()
print(f"worker {n}: {count} rows from {workers} workers, total {db.execute('select value from counters').fetchone()[0]}")
