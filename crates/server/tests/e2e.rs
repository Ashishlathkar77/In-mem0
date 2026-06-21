//! End-to-end test: boot the real server on an ephemeral port and drive it over TCP with
//! raw RESP, asserting exact wire replies. This exercises the whole stack — accept loop,
//! connection parser, command dispatch, store — together.

use inmem_server::config::Config;
use inmem_server::server::Server;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Boot a server on 127.0.0.1:0 and return its address.
fn boot() -> String {
    let cfg = Config {
        port: 0, // ephemeral
        shards: 4,
        ..Config::default()
    };
    let server = Server::bootstrap(cfg).expect("bootstrap");
    let listener = server.bind().expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let _ = server.run(listener);
    });
    addr
}

fn connect(addr: &str) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_nodelay(true).unwrap();
    s
}

/// Send raw bytes and read exactly `expect.len()` bytes back, asserting equality.
fn exchange(s: &mut TcpStream, send: &[u8], expect: &[u8]) {
    s.write_all(send).unwrap();
    let mut buf = vec![0u8; expect.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        buf,
        expect,
        "\n sent: {:?}\n got:  {:?}\n want: {:?}",
        String::from_utf8_lossy(send),
        String::from_utf8_lossy(&buf),
        String::from_utf8_lossy(expect),
    );
}

#[test]
fn full_string_workflow_over_tcp() {
    let addr = boot();
    let mut s = connect(&addr);

    // PING
    exchange(&mut s, b"*1\r\n$4\r\nPING\r\n", b"+PONG\r\n");
    // inline PING too
    exchange(&mut s, b"PING\r\n", b"+PONG\r\n");
    // SET / GET
    exchange(
        &mut s,
        b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
        b"+OK\r\n",
    );
    exchange(&mut s, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n", b"$1\r\nv\r\n");
    // GET missing -> null bulk
    exchange(&mut s, b"*2\r\n$3\r\nGET\r\n$1\r\nz\r\n", b"$-1\r\n");
    // INCR
    exchange(&mut s, b"*2\r\n$4\r\nINCR\r\n$1\r\nn\r\n", b":1\r\n");
    exchange(&mut s, b"*2\r\n$4\r\nINCR\r\n$1\r\nn\r\n", b":2\r\n");
    // EXISTS / DEL
    exchange(&mut s, b"*2\r\n$6\r\nEXISTS\r\n$1\r\nk\r\n", b":1\r\n");
    exchange(&mut s, b"*2\r\n$3\r\nDEL\r\n$1\r\nk\r\n", b":1\r\n");
    exchange(&mut s, b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n", b"$-1\r\n");
}

#[test]
fn pipelining_batches_replies() {
    let addr = boot();
    let mut s = connect(&addr);
    // three commands in one write; expect three replies concatenated
    let pipe = b"*1\r\n$4\r\nPING\r\n*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n*2\r\n$3\r\nGET\r\n$1\r\na\r\n";
    exchange(&mut s, pipe, b"+PONG\r\n+OK\r\n$1\r\n1\r\n");
}

#[test]
fn resp3_hello_and_null() {
    let addr = boot();
    let mut s = connect(&addr);
    // HELLO 3 returns a RESP3 map (starts with '%'); just check the first byte
    s.write_all(b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n").unwrap();
    let mut one = [0u8; 1];
    s.read_exact(&mut one).unwrap();
    assert_eq!(one[0], b'%', "HELLO 3 should reply with a RESP3 map");
    // drain the rest of the map reply so the stream is clean (best-effort, short read)
    let mut sink = [0u8; 4096];
    let _ = s.read(&mut sink);

    // after HELLO 3, a missing GET should be the RESP3 null `_\r\n`
    exchange(&mut s, b"*2\r\n$3\r\nGET\r\n$1\r\nq\r\n", b"_\r\n");
}
