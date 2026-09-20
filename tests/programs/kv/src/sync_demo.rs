//! Each of tokio's sync primitives under real contention, self-checking.
//! What is printed depends on the data only, never on the interleaving.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::sync::{Barrier, Mutex, Notify, OnceCell, RwLock, Semaphore};
use tokio::task::JoinSet;

const WORKERS: u64 = 6;
const ROUNDS: u64 = 40;

pub fn run() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        println!("mutex {}", mutex().await);
        println!("rwlock {}", rwlock().await);
        println!("semaphore {}", semaphore().await);
        println!("channels {}", channels().await);
        println!("broadcast {}", broadcast_and_watch().await);
        println!("notify {}", notify_barrier_once().await);
        println!("time {}", time().await);
    });
    println!("sync ok");
}

/// A read-modify-write with an await in the middle: lost updates unless the
/// lock holds across it.
async fn mutex() -> u64 {
    let total = Arc::new(Mutex::new(0u64));
    let mut set = JoinSet::new();
    for w in 0..WORKERS {
        let total = total.clone();
        set.spawn(async move {
            for i in 0..ROUNDS {
                let mut t = total.lock().await;
                let seen = *t;
                tokio::task::yield_now().await;
                *t = seen + w * i;
            }
        });
    }
    while set.join_next().await.is_some() {}
    let total = *total.lock().await;
    assert_eq!(total, (0..WORKERS).sum::<u64>() * (0..ROUNDS).sum::<u64>());
    total
}

/// Readers must never see the two halves of a write apart.
async fn rwlock() -> u64 {
    let pair = Arc::new(RwLock::new((0u64, 0u64)));
    let mut set = JoinSet::new();
    for w in 0..WORKERS {
        let pair = pair.clone();
        set.spawn(async move {
            let mut reads = 0;
            for _ in 0..ROUNDS {
                if w < 2 {
                    let mut p = pair.write().await;
                    p.0 += 1;
                    tokio::task::yield_now().await;
                    p.1 += 1;
                } else {
                    let p = pair.read().await;
                    assert_eq!(p.0, p.1, "a reader saw half a write");
                    reads += 1;
                }
            }
            reads
        });
    }
    let mut reads = 0;
    while let Some(r) = set.join_next().await {
        reads += r.unwrap();
    }
    let p = pair.read().await;
    assert_eq!(*p, (2 * ROUNDS, 2 * ROUNDS));
    reads + p.0
}

async fn semaphore() -> u64 {
    const PERMITS: u64 = 2;
    let sem = Arc::new(Semaphore::new(PERMITS as usize));
    let inside = Arc::new(AtomicU64::new(0));
    let most = Arc::new(AtomicU64::new(0));
    let mut set = JoinSet::new();
    for _ in 0..WORKERS {
        let (sem, inside, most) = (sem.clone(), inside.clone(), most.clone());
        set.spawn(async move {
            for _ in 0..ROUNDS {
                let _permit = sem.acquire().await.unwrap();
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }
    while set.join_next().await.is_some() {}
    let all = sem.acquire_many(PERMITS as u32).await.unwrap();
    drop(all);
    let most = most.load(Ordering::SeqCst);
    assert!(most <= PERMITS, "{most} holders of {PERMITS} permits");
    PERMITS
}

/// A bounded channel into a plain thread and oneshot replies back, the way
/// the server's processing thread works; and an unbounded one from a
/// blocking-pool thread.
async fn channels() -> u64 {
    let (tx, mut rx) = mpsc::channel::<(u64, oneshot::Sender<u64>)>(2);
    let squarer = std::thread::spawn(move || {
        let mut served = 0;
        while let Some((n, reply)) = rx.blocking_recv() {
            let _ = reply.send(n * n);
            served += 1;
        }
        served
    });
    let mut set = JoinSet::new();
    for w in 0..WORKERS {
        let tx = tx.clone();
        set.spawn(async move {
            let mut sum = 0;
            for i in 0..ROUNDS {
                let (reply, answer) = oneshot::channel();
                tx.send((w + i, reply)).await.unwrap();
                sum += answer.await.unwrap();
            }
            sum
        });
    }
    drop(tx);
    let mut sum = 0;
    while let Some(s) = set.join_next().await {
        sum += s.unwrap();
    }
    assert_eq!(squarer.join().unwrap(), WORKERS * ROUNDS);

    let (utx, mut urx) = mpsc::unbounded_channel();
    let producer = tokio::task::spawn_blocking(move || {
        for i in 0..ROUNDS {
            utx.send(i).unwrap();
        }
    });
    let mut got = 0;
    while let Some(i) = urx.recv().await {
        got += i;
    }
    producer.await.unwrap();
    assert_eq!(got, (0..ROUNDS).sum::<u64>());

    // A dropped sender is an error, not a hang
    let (gone, never) = oneshot::channel::<u64>();
    drop(gone);
    assert!(never.await.is_err());
    sum + got
}

async fn broadcast_and_watch() -> u64 {
    let (tx, _) = broadcast::channel::<u64>(4);
    let (level, _) = watch::channel(0u64);
    let ready = Arc::new(Barrier::new(4));
    let mut set = JoinSet::new();
    for _ in 0..3 {
        let (mut rx, mut seen, ready) = (tx.subscribe(), level.subscribe(), ready.clone());
        set.spawn(async move {
            ready.wait().await;
            let (mut sum, mut lagged) = (0, 0);
            loop {
                match rx.recv().await {
                    Ok(v) => sum += v,
                    Err(broadcast::error::RecvError::Lagged(n)) => lagged += n,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Whatever was skipped, the last value of a watch is there
            while *seen.borrow_and_update() != ROUNDS {
                seen.changed().await.unwrap();
            }
            (sum, lagged)
        });
    }
    ready.wait().await;
    for i in 1..=ROUNDS {
        tx.send(i).unwrap();
        level.send_replace(i);
        if i % 3 == 0 {
            tokio::task::yield_now().await;
        }
    }
    drop(tx);
    let mut accounted = 0;
    while let Some(r) = set.join_next().await {
        let (sum, lagged) = r.unwrap();
        assert!(sum <= (1..=ROUNDS).sum::<u64>());
        assert!(lagged < ROUNDS);
        accounted += 1;
    }
    accounted
}

async fn notify_barrier_once() -> u64 {
    static CONFIG: OnceCell<u64> = OnceCell::const_new();
    let inits = Arc::new(AtomicU64::new(0));
    let go = Arc::new(Notify::new());
    let lined_up = Arc::new(Barrier::new(WORKERS as usize + 1));
    let mut set = JoinSet::new();
    for _ in 0..WORKERS {
        let (inits, go, lined_up) = (inits.clone(), go.clone(), lined_up.clone());
        set.spawn(async move {
            let start = go.notified();
            tokio::pin!(start);
            // Registered before the barrier lets the notifier go
            start.as_mut().enable();
            let leader = lined_up.wait().await.is_leader();
            start.await;
            let v = CONFIG
                .get_or_init(|| async {
                    inits.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    42
                })
                .await;
            *v + u64::from(leader)
        });
    }
    lined_up.wait().await;
    go.notify_waiters();
    let mut sum = 0;
    while let Some(v) = set.join_next().await {
        sum += v.unwrap();
    }
    assert_eq!(inits.load(Ordering::SeqCst), 1, "OnceCell ran its init twice");
    // One leader among the waiters, unless it was us
    assert!(sum == 42 * WORKERS || sum == 42 * WORKERS + 1);
    42 * WORKERS
}

async fn time() -> u64 {
    let began = tokio::time::Instant::now();
    let slow = tokio::time::timeout(Duration::from_millis(20), std::future::pending::<()>());
    assert!(slow.await.is_err());
    let quick = tokio::time::timeout(Duration::from_secs(5), async { 7u64 });
    let seven = quick.await.unwrap();
    let mut every = tokio::time::interval(Duration::from_millis(10));
    for _ in 0..5 {
        every.tick().await;
    }
    let mut set = JoinSet::new();
    for ms in [30u64, 10, 20] {
        set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ms
        });
    }
    let mut order = Vec::new();
    while let Some(ms) = set.join_next().await {
        order.push(ms.unwrap());
    }
    assert_eq!(order, [10, 20, 30], "sleepers woke out of order");
    assert!(began.elapsed() >= Duration::from_millis(90));
    seven
}
