// std::net echo: the parent listens, starts a copy of itself as the client
// with std::process::Command, and upper-cases what the client sends.
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

fn client(port: &str) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut replies = BufReader::new(stream.try_clone().expect("clone"));
    for i in 0..5 {
        writeln!(stream, "message {i} from the client").expect("write");
        let mut line = String::new();
        replies.read_line(&mut line).expect("read");
        print!("client got: {line}");
    }
    stream.shutdown(Shutdown::Write).expect("shutdown");
    let mut rest = String::new();
    replies.read_line(&mut rest).expect("read");
    print!("client got: {rest}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "client" {
        return client(&args[2]);
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    assert!(port >= 49152, "ephemeral port expected, got {port}");
    let mut child = Command::new(std::env::current_exe().expect("current_exe"))
        .args(["client", &port.to_string()])
        .spawn()
        .expect("spawn");
    let (stream, peer) = listener.accept().expect("accept");
    println!("server accepted a connection from {}", peer.ip());
    let mut out = stream.try_clone().expect("clone");
    let mut lines = 0;
    for line in BufReader::new(stream).lines() {
        let line = line.expect("read");
        writeln!(out, "{}", line.to_uppercase()).expect("write");
        lines += 1;
    }
    writeln!(out, "goodbye after {lines} lines").expect("write");
    drop(out);
    let status = child.wait().expect("wait");
    println!("server saw {lines} lines; client exited with {status}");
}
