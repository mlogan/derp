//! A test guest on tokio.
//!
//!   kv sync [current]                 the sync primitives, in one process
//!                                     (`current`: on a current-thread runtime)
//!   kv extras [fs|signal|process]     tokio's fs, process and signal
//!   kv addrs                          `extras`, printing where the heap would
//!                                     put a block of each size at every step
//!   kv server PORT CLIENTS [LOG]      key-value server; exits after CLIENTS
//!                                     clients have said DONE
//!   kv client HOST PORT ID N          three connections doing N rounds each
//!
//! Protocol, one line each way: `SET k v`, `GET k`, `DEL k`, `INCR k id`
//! (`id` makes a resent request harmless), `HASH k`, `STATS`, `SUBSCRIBE`
//! (streams `CHANGE k` until the server shuts down), `DONE id`.

mod extras;
mod server;
mod sync_demo;

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(1) {
        Some("sync") => sync_demo::run(arg(2) == Some("current")),
        Some("extras") => extras::run(arg(2)),
        Some("addrs") => extras::run_probing(),
        Some("child") => {
            println!("child {} says hi", arg(2).unwrap_or("?"));
            std::process::exit(3);
        }
        Some("server") => {
            let port = arg(2).and_then(|p| p.parse().ok()).expect("PORT");
            let clients = arg(3).and_then(|c| c.parse().ok()).expect("CLIENTS");
            server::run(port, clients, arg(4).map(Into::into));
        }
        Some("client") => {
            let (host, port) = (arg(2).expect("HOST"), arg(3).expect("PORT"));
            let id: u32 = arg(4).and_then(|v| v.parse().ok()).expect("ID");
            let rounds: u32 = arg(5).and_then(|v| v.parse().ok()).expect("N");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(client(format!("{host}:{port}"), id, rounds));
        }
        _ => {
            eprintln!(
                "usage: kv sync [current] | extras | server PORT CLIENTS [LOG] | client HOST PORT ID N"
            );
            std::process::exit(2);
        }
    }
}

pub fn checksum(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One connection that outlives the server's crashes: a request that gets
/// no answer is sent again on a new connection.
struct Conn {
    addr: String,
    stream: Option<BufReader<TcpStream>>,
    reconnects: u32,
}

impl Conn {
    async fn request(&mut self, line: &str) -> String {
        loop {
            if self.stream.is_none() {
                match TcpStream::connect(&self.addr).await {
                    Ok(s) => self.stream = Some(BufReader::new(s)),
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                }
            }
            let stream = self.stream.as_mut().unwrap();
            let mut reply = String::new();
            let sent = stream
                .get_mut()
                .write_all(format!("{line}\n").as_bytes())
                .await;
            if sent.is_ok() && matches!(stream.read_line(&mut reply).await, Ok(n) if n > 0) {
                return reply.trim_end().to_string();
            }
            self.stream = None;
            self.reconnects += 1;
        }
    }
}

async fn client(addr: String, id: u32, rounds: u32) {
    const TASKS: u32 = 3;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(TASKS as usize));
    let mut tasks = tokio::task::JoinSet::new();
    for t in 0..TASKS {
        let (addr, barrier) = (addr.clone(), barrier.clone());
        tasks.spawn(async move {
            let mut c = Conn {
                addr,
                stream: None,
                reconnects: 0,
            };
            // Connected before the start, so that all three race
            assert!(c.request("STATS").await.starts_with("STATS"));
            barrier.wait().await;
            for i in 0..rounds {
                let (key, value) = (format!("c{id}t{t}k{}", i % 5), format!("v{i}"));
                assert_eq!(c.request(&format!("SET {key} {value}")).await, "OK");
                assert_eq!(
                    c.request(&format!("GET {key}")).await,
                    format!("VALUE {value}")
                );
                let n = c.request(&format!("INCR counter{id} c{id}t{t}i{i}")).await;
                assert!(n.starts_with("VALUE "), "{n}");
                c.request(&format!("INCR total c{id}t{t}i{i}T")).await;
                if i % 4 == 0 {
                    let h = c.request(&format!("HASH {key}")).await;
                    assert_eq!(h, format!("HASH {}", checksum(&value)));
                }
                if i % 7 == 6 {
                    assert_eq!(c.request(&format!("DEL {key}")).await, "OK");
                    assert_eq!(c.request(&format!("GET {key}")).await, "NONE");
                }
            }
            c.reconnects
        });
    }
    // Watches the changes go by until the server ends the stream. A server
    // that dies takes the subscription with it: subscribe again, unless the
    // client is finished and the server may be gone for good.
    let (finished, mut is_finished) = tokio::sync::watch::channel(false);
    let watcher = {
        let addr = addr.clone();
        tokio::spawn(async move {
            let (mut changes, mut line) = (0u32, String::new());
            loop {
                let mut c = Conn {
                    addr: addr.clone(),
                    stream: None,
                    reconnects: 0,
                };
                tokio::select! {
                    reply = c.request("SUBSCRIBE") => assert_eq!(reply, "OK"),
                    _ = is_finished.wait_for(|&f| f) => return changes,
                }
                let mut stream = c.stream.take().unwrap();
                loop {
                    line.clear();
                    if !matches!(stream.read_line(&mut line).await, Ok(n) if n > 0) {
                        break;
                    }
                    match line.trim_end() {
                        "END" => return changes,
                        l => assert!(l.starts_with("CHANGE ") || l.starts_with("LAGGED "), "{l}"),
                    }
                    changes += 1;
                }
                if *is_finished.borrow() {
                    return changes;
                }
            }
        })
    };
    let mut reconnects = 0;
    while let Some(r) = tasks.join_next().await {
        reconnects += r.expect("client task");
    }
    let mut c = Conn {
        addr,
        stream: None,
        reconnects: 0,
    };
    let counter = c.request(&format!("GET counter{id}")).await;
    assert_eq!(counter, format!("VALUE {}", TASKS * rounds));
    assert_eq!(c.request(&format!("DONE {id}")).await, "OK");
    finished.send_replace(true);
    let changes = watcher.await.expect("watcher");
    assert!(changes > 0);
    println!("client {id}: {counter} reconnects={reconnects} changes={changes}");
}
