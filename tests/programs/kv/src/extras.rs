//! The parts of tokio that lean on the operating system rather than on
//! atomics: files (through the blocking pool), child processes (spawn,
//! pipes, exit status through the SIGCHLD machinery) and signals.

use tokio::io::AsyncReadExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinSet;

/// Sizes that land in distinct classes of a size-class allocator
const PROBE_SIZES: [usize; 14] = [
    16, 32, 48, 64, 96, 112, 128, 144, 192, 256, 512, 1024, 4096, 65536,
];

/// Where a block of each size would go right now. Printed, so that any
/// difference in the heap's history shows up as a difference in output,
/// with the phase and the size class that moved. Blocks are released in
/// reverse, which leaves a free-list allocator as it was found.
pub fn probe(phase: &str) -> String {
    let blocks: Vec<Vec<u8>> = PROBE_SIZES.iter().map(|&n| Vec::with_capacity(n)).collect();
    let mut line = format!("heap {phase}:");
    for b in &blocks {
        line.push_str(&format!(" {:x}", b.as_ptr() as usize));
    }
    for b in blocks.into_iter().rev() {
        drop(b);
    }
    line
}

/// Set by `kv addrs`: print the probes. Off, the modes print only what is
/// the same natively.
static PROBING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn checkpoint(phase: &str) {
    if PROBING.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{}", probe(phase));
    }
}

/// `kv extras` with the heap probed at every step.
pub fn run_probing() {
    PROBING.store(true, std::sync::atomic::Ordering::Relaxed);
    checkpoint("start");
    run(None);
    checkpoint("end");
}

pub fn run(only: Option<&str>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    checkpoint("runtime up");
    rt.block_on(async {
        if only.is_none_or(|o| o == "fs") {
            println!("fs {}", fs().await);
            checkpoint("after fs");
        }
        if only.is_none_or(|o| o == "signal") {
            println!("signal {}", signals().await);
            checkpoint("after signal");
        }
        if only.is_none_or(|o| o == "process") {
            println!("process {}", processes().await);
            checkpoint("after process");
        }
    });
    drop(rt);
    checkpoint("runtime down");
    println!("extras ok");
}

async fn fs() -> usize {
    tokio::fs::create_dir_all("extras/sub").await.unwrap();
    let mut set = JoinSet::new();
    for i in 0..6u32 {
        set.spawn(async move {
            let (tmp, path) = (format!("extras/sub/f{i}.tmp"), format!("extras/sub/f{i}"));
            tokio::fs::write(&tmp, format!("file {i}\n").repeat(100))
                .await
                .unwrap();
            tokio::fs::rename(&tmp, &path).await.unwrap();
            let n = tokio::fs::read_to_string(&path).await.unwrap().len();
            (n, i, probe(&format!("in fs task {i}")))
        });
    }
    let mut bytes = 0;
    let mut probes = Vec::new();
    while let Some(done) = set.join_next().await {
        let (n, i, line) = done.unwrap();
        bytes += n;
        probes.push((i, line));
    }
    probes.sort();
    for (_, line) in &probes {
        if PROBING.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{line}");
        }
    }
    let mut names = Vec::new();
    let mut dir = tokio::fs::read_dir("extras/sub").await.unwrap();
    while let Some(entry) = dir.next_entry().await.unwrap() {
        names.push(entry.file_name().into_string().unwrap());
    }
    names.sort();
    assert_eq!(names, ["f0", "f1", "f2", "f3", "f4", "f5"]);
    tokio::fs::remove_dir_all("extras").await.unwrap();
    assert!(tokio::fs::metadata("extras").await.is_err());
    bytes
}

async fn signals() -> u32 {
    let mut usr1 = signal(SignalKind::user_defined1()).unwrap();
    let mut usr2 = signal(SignalKind::user_defined2()).unwrap();
    let mut seen = 0;
    for round in 0..5 {
        let which = if round % 2 == 0 {
            libc::SIGUSR1
        } else {
            libc::SIGUSR2
        };
        unsafe { libc::kill(libc::getpid(), which) };
        tokio::select! {
            Some(()) = usr1.recv() => assert_eq!(which, libc::SIGUSR1),
            Some(()) = usr2.recv() => assert_eq!(which, libc::SIGUSR2),
        }
        seen += 1;
    }
    seen
}

async fn processes() -> i32 {
    let me = std::env::current_exe().unwrap();
    let mut set = JoinSet::new();
    for i in 0..3 {
        let me = me.clone();
        set.spawn(async move {
            let mut child = tokio::process::Command::new(me)
                .args(["child", &i.to_string()])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut said = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut said)
                .await
                .unwrap();
            assert_eq!(said, format!("child {i} says hi\n"));
            let code = child.wait().await.unwrap().code().unwrap();
            (code, i, probe(&format!("after child {i}")))
        });
    }
    let mut codes = 0;
    let mut probes = Vec::new();
    while let Some(done) = set.join_next().await {
        let (code, i, line) = done.unwrap();
        codes += code;
        probes.push((i, line));
    }
    probes.sort();
    for (_, line) in &probes {
        if PROBING.load(std::sync::atomic::Ordering::Relaxed) {
            println!("{line}");
        }
    }
    assert_eq!(codes, 9);
    codes
}
