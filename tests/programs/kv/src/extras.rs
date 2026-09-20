//! The parts of tokio that lean on the operating system rather than on
//! atomics: files (through the blocking pool), child processes (spawn,
//! pipes, exit status through the SIGCHLD machinery) and signals.

use tokio::io::AsyncReadExt;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinSet;

pub fn run(only: Option<&str>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        if only.is_none_or(|o| o == "fs") {
            println!("fs {}", fs().await);
        }
        if only.is_none_or(|o| o == "signal") {
            println!("signal {}", signals().await);
        }
        if only.is_none_or(|o| o == "process") {
            println!("process {}", processes().await);
        }
    });
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
            tokio::fs::read_to_string(&path).await.unwrap().len()
        });
    }
    let mut bytes = 0;
    while let Some(n) = set.join_next().await {
        bytes += n.unwrap();
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
            child.wait().await.unwrap().code().unwrap()
        });
    }
    let mut codes = 0;
    while let Some(code) = set.join_next().await {
        codes += code.unwrap();
    }
    assert_eq!(codes, 9);
    codes
}
