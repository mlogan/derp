// Three worker threads feed an mpsc channel; the receiver tallies words in a
// HashMap and prints them sorted, so the output is deterministic.
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

fn main() {
    let (tx, rx) = mpsc::channel::<(usize, String)>();
    let mut handles = Vec::new();
    for w in 0..3 {
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            for i in 0..2000 {
                let word = format!("w{}", (i * (w + 7)) % 13);
                tx.send((w, word)).unwrap();
            }
        }));
    }
    drop(tx);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut per_worker = [0usize; 3];
    for (w, word) in rx {
        *counts.entry(word).or_default() += 1;
        per_worker[w] += 1;
    }
    for h in handles {
        h.join().unwrap();
    }
    let mut keys: Vec<_> = counts.iter().collect();
    keys.sort();
    for (k, v) in keys {
        println!("{k} {v}");
    }
    println!("per_worker {per_worker:?}");
}
