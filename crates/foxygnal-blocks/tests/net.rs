//! Network blocks over loopback. Every test picks its own ephemeral ports, so
//! they can run in parallel.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use foxygnal_blocks::testing::{Collected, VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, Instance, Params, Running, Sample, Tag, TagValue};
use serde_json::{Value, json};

fn build(kind: &str, p: Value) -> Instance {
    let p: Params = p.as_object().unwrap().clone();
    foxygnal_blocks::registry().build(kind, &p).unwrap()
}

fn tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Run a chain of blocks.
fn chain(blocks: Vec<Instance>) -> Running {
    let mut fg = Flowgraph::new();
    let ids: Vec<_> = blocks
        .into_iter()
        .enumerate()
        .map(|(i, b)| fg.add(format!("b{i}"), b))
        .collect();
    for w in ids.windows(2) {
        fg.connect(w[0], 0, w[1], 0).unwrap();
    }
    fg.start().unwrap()
}

/// Wait for a graph to finish on its own.
fn finish(r: Running, within: Duration) {
    let t0 = Instant::now();
    while !r.is_finished() {
        assert!(
            t0.elapsed() < within,
            "graph did not finish within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    r.wait().unwrap();
}

/// Stop a running graph; it must stop promptly (no block may be stuck in `work`).
fn stop(r: Running) {
    let t0 = Instant::now();
    r.stop();
    r.wait().unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "stopping took {:?}",
        t0.elapsed()
    );
}

fn wait_for<T>(data: &Collected<T>, n: usize, within: Duration) {
    let t0 = Instant::now();
    while data.lock().unwrap().len() < n {
        assert!(
            t0.elapsed() < within,
            "got {} of {n} items",
            data.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait until a `udp_source` listens on `port` (blocks start on their own
/// thread, after `Flowgraph::start` returns). Probes with empty datagrams,
/// which carry no items under any header: an unbound port answers with an
/// ICMP error, seen as `ConnectionRefused` on a connected socket.
fn wait_listening(port: u16) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let t0 = Instant::now();
    let mut quiet = 0;
    while quiet < 3 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "udp_source never listened on {port}"
        );
        let _ = s.send(&[]);
        match s.recv(&mut [0u8; 1]) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => quiet = 0,
            _ => quiet += 1,
        }
    }
}

fn tags_named<'a>(tags: &'a [Tag], key: &str) -> Vec<&'a Tag> {
    tags.iter().filter(|t| &*t.key == key).collect()
}

fn ramp_f32(n: usize) -> Vec<f32> {
    (0..n).map(|i| i as f32 * 0.25 - 1000.0).collect()
}

fn ramp_c32(n: usize) -> Vec<Complex32> {
    (0..n)
        .map(|i| Complex32::new(i as f32, -(i as f32) * 0.5))
        .collect()
}

/// udp_sink → udp_source with a sequence header and an odd datagram size.
fn udp_round_trip<T: Sample + PartialEq + std::fmt::Debug>(ty: &str, data: Vec<T>) {
    let port = udp_port();
    let n = data.len();
    let (sink, got, tags) = VectorSink::<T>::with_tags();
    let rx = chain(vec![
        build(
            "udp_source",
            json!({"type": ty, "port": port, "header": "seq64"}),
        ),
        sink,
    ]);
    wait_listening(port);
    let tx = chain(vec![
        VectorSource::instance(data.clone()),
        // Paced, but the throttle passes 10 ms batches and catches up in one
        // burst after a late wakeup: the data as a whole must fit the
        // receiver's socket buffer (Linux's default, 208 KiB of datagram
        // memory, holds about 90 of these) or a busy machine drops some.
        build("throttle", json!({"type": ty, "sample_rate": 1e5})),
        build(
            "udp_sink",
            json!({"type": ty, "port": port, "header": "seq64", "payload": 1001}),
        ),
    ]);
    finish(tx, Duration::from_secs(10));
    wait_for(&got, n, Duration::from_secs(5));
    stop(rx);
    assert_eq!(*got.lock().unwrap(), data);
    assert!(tags_named(&tags.lock().unwrap(), "dropped").is_empty());
}

#[test]
fn udp_round_trip_f32() {
    udp_round_trip("f32", ramp_f32(12_000));
}

#[test]
fn udp_round_trip_c32() {
    udp_round_trip("c32", ramp_c32(6_000));
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn udp_sequence_gap_is_tagged() {
    let port = udp_port();
    let (sink, got, tags) = VectorSink::<f32>::with_tags();
    let rx = chain(vec![
        build(
            "udp_source",
            json!({"type": "f32", "port": port, "header": "seq64"}),
        ),
        sink,
    ]);
    wait_listening(port);
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    // Datagram k carries 4 items k*10 .. k*10+3. Datagram 2 goes missing,
    // datagram 1 arrives twice (a duplicate is dropped).
    for k in [0u64, 1, 1, 3, 4] {
        let mut pkt = k.to_le_bytes().to_vec();
        let items: Vec<f32> = (0..4).map(|i| (k * 10 + i) as f32).collect();
        pkt.extend(f32_bytes(&items));
        s.send_to(&pkt, ("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    wait_for(&got, 16, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(50));
    stop(rx);
    let want: Vec<f32> = [0, 1, 3, 4]
        .iter()
        .flat_map(|k| (0..4).map(move |i| (k * 10 + i) as f32))
        .collect();
    assert_eq!(*got.lock().unwrap(), want);
    let tags = tags.lock().unwrap();
    let dropped = tags_named(&tags, "dropped");
    assert_eq!(dropped.len(), 1, "{tags:?}");
    assert_eq!(dropped[0].offset, 8);
    assert_eq!(dropped[0].value, TagValue::Int(4));
}

#[test]
fn udp_items_split_across_datagrams() {
    let port = udp_port();
    let (sink, got) = VectorSink::<f32>::instance();
    let rx = chain(vec![
        build("udp_source", json!({"type": "f32", "port": port})),
        sink,
    ]);
    wait_listening(port);
    let data = ramp_f32(100);
    let bytes = f32_bytes(&data);
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut off = 0;
    for len in [3usize, 5, 1, 7, 13].iter().cycle() {
        if off >= bytes.len() {
            break;
        }
        let end = (off + len).min(bytes.len());
        s.send_to(&bytes[off..end], ("127.0.0.1", port)).unwrap();
        off = end;
    }
    wait_for(&got, data.len(), Duration::from_secs(5));
    stop(rx);
    assert_eq!(*got.lock().unwrap(), data);
}

#[test]
fn tcp_sink_server_to_tcp_source_client() {
    let port = tcp_port();
    let data = ramp_c32(200_000);
    let tx = chain(vec![
        VectorSource::instance(data.clone()),
        // Backpressure: wait for the receiver instead of discarding.
        build(
            "tcp_sink",
            json!({"type": "c32", "port": port, "backpressure": true}),
        ),
    ]);
    std::thread::sleep(Duration::from_millis(150));
    let (sink, got, tags) = VectorSink::<Complex32>::with_tags();
    let rx = chain(vec![
        build(
            "tcp_source",
            json!({"type": "c32", "mode": "client", "port": port, "reconnect": false}),
        ),
        sink,
    ]);
    finish(tx, Duration::from_secs(10));
    // The sink closes the connection at the end: the source's stream ends.
    finish(rx, Duration::from_secs(10));
    assert_eq!(got.lock().unwrap().len(), data.len());
    assert_eq!(*got.lock().unwrap(), data);
    let tags = tags.lock().unwrap();
    let c = tags_named(&tags, "connected");
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].offset, &c[0].value), (0, &TagValue::Int(1)));
}

#[test]
fn tcp_sink_client_to_tcp_source_server() {
    let port = tcp_port();
    let (sink, got) = VectorSink::<f32>::instance();
    let rx = chain(vec![
        build(
            "tcp_source",
            json!({"type": "f32", "port": port, "reconnect": false}),
        ),
        sink,
    ]);
    std::thread::sleep(Duration::from_millis(150));
    let data = ramp_f32(300_001);
    let tx = chain(vec![
        VectorSource::instance(data.clone()),
        build(
            "tcp_sink",
            json!({"type": "f32", "mode": "client", "port": port, "backpressure": true,
                   "retry": 0.05}),
        ),
    ]);
    finish(tx, Duration::from_secs(10));
    finish(rx, Duration::from_secs(10));
    assert_eq!(*got.lock().unwrap(), data);
}

#[test]
fn tcp_source_server_takes_the_next_client() {
    let port = tcp_port();
    let (sink, got, tags) = VectorSink::<f32>::with_tags();
    let rx = chain(vec![
        build("tcp_source", json!({"type": "f32", "port": port})),
        sink,
    ]);
    std::thread::sleep(Duration::from_millis(150));
    let a = ramp_f32(1000);
    let b: Vec<f32> = (0..500).map(|i| i as f32).collect();
    {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut bytes = f32_bytes(&a);
        bytes.extend([1, 2]); // a partial item: dropped with the connection
        s.write_all(&bytes).unwrap();
    }
    wait_for(&got, a.len(), Duration::from_secs(5));
    {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(&f32_bytes(&b)).unwrap();
    }
    wait_for(&got, a.len() + b.len(), Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!rx.is_finished(), "reconnect keeps the stream going");
    stop(rx);
    let mut want = a.clone();
    want.extend(&b);
    assert_eq!(*got.lock().unwrap(), want);
    let tags = tags.lock().unwrap();
    let c: Vec<_> = tags_named(&tags, "connected")
        .iter()
        .map(|t| (t.offset, t.value.clone()))
        .collect();
    assert_eq!(
        c,
        vec![(0, TagValue::Int(1)), (a.len() as u64, TagValue::Int(2))]
    );
}

#[test]
fn tcp_source_client_reconnects() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (sink, got) = VectorSink::<f32>::instance();
    let rx = chain(vec![
        build(
            "tcp_source",
            json!({"type": "f32", "mode": "client", "port": port, "retry": 0.05}),
        ),
        sink,
    ]);
    let a = ramp_f32(777);
    let b: Vec<f32> = (0..333).map(|i| -(i as f32)).collect();
    for part in [&a, &b] {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(&f32_bytes(part)).unwrap();
        // Hang up; the source connects again.
    }
    wait_for(&got, a.len() + b.len(), Duration::from_secs(5));
    stop(rx);
    let mut want = a.clone();
    want.extend(&b);
    assert_eq!(*got.lock().unwrap(), want);
}

#[test]
fn tcp_source_without_server_fails_without_reconnect() {
    let port = tcp_port(); // nobody listens here
    let (sink, _) = VectorSink::<f32>::instance();
    let rx = chain(vec![
        build(
            "tcp_source",
            json!({"type": "f32", "mode": "client", "port": port, "reconnect": false}),
        ),
        sink,
    ]);
    let t0 = Instant::now();
    while !rx.is_finished() {
        assert!(t0.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let err = rx.wait().unwrap_err().to_string();
    assert!(err.contains("cannot connect"), "{err}");
}

/// A 1 s, 4 MS/s c32 stream (32 MB) into a server-mode tcp_sink.
fn paced_into_tcp_sink(port: u16, extra: Value) -> Running {
    let mut p = json!({"type": "c32", "port": port, "queue": 64});
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    chain(vec![
        build(
            "signal_source",
            json!({"type": "c32", "frequency": 1000.0, "sample_rate": 4e6}),
        ),
        build("throttle", json!({"type": "c32"})),
        build("head", json!({"type": "c32", "count": 4_000_000})),
        build("tcp_sink", p),
    ])
}

#[test]
fn slow_tcp_client_does_not_stall_the_graph() {
    let port = tcp_port();
    let tx = paced_into_tcp_sink(port, json!({}));
    std::thread::sleep(Duration::from_millis(100));
    // One client that never reads, one that reads everything.
    let slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut fast = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        let mut total = 0usize;
        loop {
            match fast.read(&mut buf) {
                Ok(0) | Err(_) => return total,
                Ok(n) => total += n,
            }
        }
    });
    finish(tx, Duration::from_secs(10));
    let fast_total = reader.join().unwrap();
    assert!(fast_total > 1_000_000, "fast client got {fast_total} bytes");
    assert_eq!(fast_total % 8, 0, "only whole items are sent");
    drop(slow);
}

#[test]
fn backpressure_waits_for_a_slow_client_and_still_stops() {
    let port = tcp_port();
    let tx = paced_into_tcp_sink(port, json!({"backpressure": true}));
    std::thread::sleep(Duration::from_millis(100));
    let slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !tx.is_finished(),
        "the graph should wait for the slow client"
    );
    stop(tx);
    drop(slow);
}

#[test]
fn udp_loopback_example_builds() {
    let spec = foxygnal_core::GraphSpec::from_toml(include_str!(
        "../../../examples/udp-loopback.fox.toml"
    ))
    .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    assert!(edges.iter().all(|e| e.meta.sample_rate == Some(48000.0)));
}

#[test]
fn idle_sources_stop_promptly() {
    let (s1, _) = VectorSink::<f32>::instance();
    let (s2, _) = VectorSink::<f32>::instance();
    let (s3, _) = VectorSink::<f32>::instance();
    let a = chain(vec![
        build("udp_source", json!({"type": "f32", "port": udp_port()})),
        s1,
    ]);
    let b = chain(vec![
        build("tcp_source", json!({"type": "f32", "port": tcp_port()})),
        s2,
    ]);
    let c = chain(vec![
        build(
            "tcp_source",
            json!({"type": "f32", "mode": "client", "port": tcp_port(), "retry": 0.05}),
        ),
        s3,
    ]);
    std::thread::sleep(Duration::from_millis(200));
    for r in [a, b, c] {
        assert!(!r.is_finished());
        stop(r);
    }
}
