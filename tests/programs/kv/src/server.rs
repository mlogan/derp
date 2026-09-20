use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, Notify, RwLock, Semaphore};

#[derive(Default, Debug)]
struct Stats {
    sets: u64,
    gets: u64,
    dels: u64,
    incrs: u64,
    hashes: u64,
}

struct Store {
    /// Read directly by connections; written only by the processing thread
    map: RwLock<HashMap<String, String>>,
    stats: Mutex<Stats>,
    in_flight: Semaphore,
    changes: broadcast::Sender<String>,
    shutdown: watch::Sender<bool>,
    /// The processing thread has seen every client's DONE
    all_done: Notify,
    ticks: std::sync::atomic::AtomicU64,
}

enum Write {
    Set(String, String),
    Del(String),
    Incr(String, String),
    Done(String),
}

struct Job {
    write: Write,
    reply: oneshot::Sender<String>,
}

const MAX_IN_FLIGHT: usize = 4;

pub fn run(port: u16, clients: usize, log: Option<PathBuf>) {
    let (changes, _) = broadcast::channel(16);
    let (shutdown, _) = watch::channel(false);
    let store = Arc::new(Store {
        map: RwLock::new(HashMap::new()),
        stats: Mutex::new(Stats::default()),
        in_flight: Semaphore::new(MAX_IN_FLIGHT),
        changes,
        shutdown,
        all_done: Notify::new(),
        ticks: 0.into(),
    });
    // Small on purpose: connections feel the back-pressure
    let (jobs, inbox) = mpsc::channel::<Job>(2);
    let processor = {
        let store = store.clone();
        std::thread::spawn(move || process(&store, inbox, clients, log))
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(serve(port, store.clone(), jobs));
    drop(rt);
    processor.join().unwrap();
    let map = store.map.blocking_read();
    let done = map.keys().filter(|k| k.starts_with("__done:")).count();
    println!(
        "server: total={} done={done} {:?}",
        map.get("total").map_or("0", String::as_str),
        store.stats.blocking_lock()
    );
}

/// The one writer. Every change is on disk before it is applied or
/// answered, so a restarted server comes back with what it acknowledged.
fn process(store: &Store, mut inbox: mpsc::Receiver<Job>, clients: usize, log: Option<PathBuf>) {
    let mut file = log.map(|path| {
        if let Ok(old) = std::fs::read_to_string(&path) {
            let mut map = store.map.blocking_write();
            for line in old.lines() {
                match line.split(' ').collect::<Vec<_>>()[..] {
                    ["SET", k, v] => drop(map.insert(k.into(), v.into())),
                    ["DEL", k] => drop(map.remove(k)),
                    _ => {}
                }
            }
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap()
    });
    while let Some(Job { write, reply }) = inbox.blocking_recv() {
        let mut map = store.map.blocking_write();
        let (changed, answer, record) = match write {
            Write::Set(k, v) => (k.clone(), "OK".to_string(), format!("SET {k} {v}\n")),
            Write::Del(k) => (k.clone(), "OK".to_string(), format!("DEL {k}\n")),
            Write::Done(id) => (format!("__done:{id}"), "OK".to_string(), format!("SET __done:{id} 1\n")),
            Write::Incr(k, id) => {
                let marker = format!("__applied:{id}");
                if let Some(earlier) = map.get(&marker) {
                    let _ = reply.send(format!("VALUE {earlier}"));
                    continue;
                }
                let n = map.get(&k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + 1;
                (k.clone(), format!("VALUE {n}"), format!("SET {k} {n}\nSET {marker} {n}\n"))
            }
        };
        if let Some(f) = file.as_mut() {
            f.write_all(record.as_bytes()).unwrap();
        }
        for line in record.lines() {
            match line.split(' ').collect::<Vec<_>>()[..] {
                ["SET", k, v] => drop(map.insert(k.into(), v.into())),
                ["DEL", k] => drop(map.remove(k)),
                _ => unreachable!(),
            }
        }
        let done = map.keys().filter(|k| k.starts_with("__done:")).count();
        drop(map);
        let _ = store.changes.send(changed);
        let _ = reply.send(answer);
        if done >= clients {
            store.all_done.notify_one();
        }
    }
}

async fn serve(port: u16, store: Arc<Store>, jobs: mpsc::Sender<Job>) {
    let listener = TcpListener::bind(("0.0.0.0", port)).await.expect("bind");
    let ticker = {
        let store = store.clone();
        let mut stop = store.shutdown.subscribe();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    _ = every.tick() => {
                        store.ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ = stop.changed() => return,
                }
            }
        })
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.expect("accept");
                connections.spawn(connection(stream, store.clone(), jobs.clone()));
            }
            () = store.all_done.notified() => break,
        }
    }
    store.shutdown.send_replace(true);
    // Subscribers end on the signal; anyone else has a moment to hang up
    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    });
    if drained.await.is_err() {
        connections.shutdown().await;
    }
    ticker.await.unwrap();
    assert!(store.ticks.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

async fn connection(stream: TcpStream, store: Arc<Store>, jobs: mpsc::Sender<Job>) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let mut stop = store.shutdown.subscribe();
    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                _ => return,
            },
            _ = stop.changed() => return,
        };
        let words: Vec<&str> = line.split(' ').collect();
        let _permit = store.in_flight.acquire().await.unwrap();
        let write = match words[..] {
            ["SET", k, v] => {
                store.stats.lock().await.sets += 1;
                Write::Set(k.into(), v.into())
            }
            ["DEL", k] => {
                store.stats.lock().await.dels += 1;
                Write::Del(k.into())
            }
            ["INCR", k, id] => {
                store.stats.lock().await.incrs += 1;
                Write::Incr(k.into(), id.into())
            }
            ["DONE", id] => Write::Done(id.into()),
            ["SUBSCRIBE"] => {
                drop(_permit);
                return subscription(&store, &mut wr).await;
            }
            _ => {
                let answer = read_only(&store, &words).await;
                if wr.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let (reply, answer) = oneshot::channel();
        if jobs.send(Job { write, reply }).await.is_err() {
            return;
        }
        let Ok(answer) = answer.await else { return };
        if wr.write_all(format!("{answer}\n").as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn read_only(store: &Arc<Store>, words: &[&str]) -> String {
    match words {
        ["GET", k] => {
            store.stats.lock().await.gets += 1;
            match store.map.read().await.get(*k) {
                Some(v) => format!("VALUE {v}"),
                None => "NONE".into(),
            }
        }
        ["HASH", k] => {
            store.stats.lock().await.hashes += 1;
            let Some(value) = store.map.read().await.get(*k).cloned() else {
                return "NONE".into();
            };
            // Off the async workers, as real CPU-bound work would be
            let sum = tokio::task::spawn_blocking(move || crate::checksum(&value));
            format!("HASH {}", sum.await.unwrap())
        }
        ["STATS"] => format!("STATS {:?}", store.stats.lock().await),
        _ => "ERR".into(),
    }
}

async fn subscription(store: &Arc<Store>, wr: &mut tokio::net::tcp::OwnedWriteHalf) {
    let mut changes = store.changes.subscribe();
    let mut stop = store.shutdown.subscribe();
    let _ = wr.write_all(b"OK\n").await;
    loop {
        let line = tokio::select! {
            change = changes.recv() => match change {
                Ok(key) => format!("CHANGE {key}\n"),
                Err(broadcast::error::RecvError::Lagged(n)) => format!("LAGGED {n}\n"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = stop.changed() => break,
        };
        if wr.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
    let _ = wr.write_all(b"END\n").await;
}
