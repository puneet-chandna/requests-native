//! In-process loopback micro-benchmark for the `requests-native` core client.
//!
//! Unlike `benchmarks/run.py`, this spawns its own minimal HTTP/1.1 fixture on a
//! background std thread, so measurements only contain client-side cost plus one
//! loopback round trip. It reports per-phase percentiles and a raw-socket
//! baseline so the client's overhead over a hand-written request is visible.
//!
//! ```console
//! cargo run --release --manifest-path benchmarks/rust-native/Cargo.toml \
//!     --bin core_loopback -- --requests 20000 --concurrency 1
//! ```

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

type DynError = Box<dyn std::error::Error + Send + Sync>;

const CHUNK: usize = 16 * 1024;

struct Options {
    requests: usize,
    concurrency: usize,
    warmup: usize,
    body_bytes: usize,
    mode: String,
    surface: String,
    runtime: String,
}

fn parse_options() -> Result<Options, DynError> {
    let mut options = Options {
        requests: 20_000,
        concurrency: 1,
        warmup: 200,
        body_bytes: 1536,
        mode: "pooled".to_owned(),
        surface: "async".to_owned(),
        runtime: "multi".to_owned(),
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(flag) = arguments.next() {
        let mut value = || arguments.next().ok_or("missing value for flag");
        match flag.as_str() {
            "--requests" => options.requests = value()?.parse()?,
            "--concurrency" => options.concurrency = value()?.parse()?,
            "--warmup" => options.warmup = value()?.parse()?,
            "--body-bytes" => options.body_bytes = value()?.parse()?,
            "--mode" => options.mode = value()?,
            "--surface" => options.surface = value()?,
            "--runtime" => options.runtime = value()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    if !matches!(options.mode.as_str(), "pooled" | "one-shot")
        || !matches!(options.surface.as_str(), "async" | "blocking" | "raw")
        || !matches!(options.runtime.as_str(), "multi" | "current")
    {
        return Err("invalid mode/surface/runtime".into());
    }
    Ok(options)
}

/// Minimal HTTP/1.1 keep-alive fixture: one thread per accepted connection.
fn spawn_fixture(body: Arc<Vec<u8>>) -> Result<u16, DynError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let body = Arc::clone(&body);
            std::thread::spawn(move || serve_connection(stream, &body));
        }
    });
    Ok(port)
}

static FIXTURE_REQUESTS: AtomicU64 = AtomicU64::new(0);

fn serve_connection(mut stream: TcpStream, body: &[u8]) {
    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
    let mut reader = BufReader::with_capacity(16 * 1024, stream.try_clone().expect("clone"));
    let mut scratch = Vec::with_capacity(1024);
    loop {
        scratch.clear();
        loop {
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Ok(1) => {}
                    _ => return,
                }
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    break;
                }
            }
            scratch.extend_from_slice(&line);
            if line == b"\r\n" {
                break;
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nX-Fixture: 1\r\n\r\n",
            body.len()
        );
        // Coalesced single write, like a real server that has the whole response ready.
        let mut response = Vec::with_capacity(head.len() + body.len());
        response.extend_from_slice(head.as_bytes());
        response.extend_from_slice(body);
        if stream.write_all(&response).is_err() || stream.flush().is_err() {
            return;
        }
        FIXTURE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn percentile(sorted: &[u64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index] as f64 / 1000.0
}

struct Report {
    label: String,
    requests: usize,
    elapsed: Duration,
    totals: Vec<u64>,
    heads: Vec<u64>,
    bodies: Vec<u64>,
}

impl Report {
    fn print(self, body_bytes: usize) {
        let mut totals = self.totals.clone();
        let mut heads = self.heads.clone();
        let mut bodies = self.bodies.clone();
        totals.sort_unstable();
        heads.sort_unstable();
        bodies.sort_unstable();
        let seconds = self.elapsed.as_secs_f64();
        println!(
            "{label}: {requests} requests in {seconds:.3} s -> {throughput:.0} req/s ({mbps:.0} MiB/s)",
            label = self.label,
            requests = self.requests,
            throughput = self.requests as f64 / seconds,
            mbps = (self.requests * body_bytes) as f64 / seconds / (1024.0 * 1024.0),
        );
        for (name, values) in [("total", &totals), ("head", &heads), ("body", &bodies)] {
            println!(
                "  {name:6} p50={:7.1}us p90={:7.1}us p99={:7.1}us max={:8.1}us",
                percentile(values, 0.50),
                percentile(values, 0.90),
                percentile(values, 0.99),
                values.last().copied().unwrap_or(0) as f64 / 1000.0,
            );
        }
    }
}

async fn measure_async(options: &Options, port: u16) -> Result<Report, DynError> {
    let url = format!("http://127.0.0.1:{port}/small");
    let pooled = (options.mode == "pooled")
        .then(requests::Client::new)
        .transpose()?;
    let mut totals = Vec::with_capacity(options.requests);
    let mut heads = Vec::with_capacity(options.requests);
    let mut bodies = Vec::with_capacity(options.requests);
    for index in 0..options.warmup + options.requests {
        let one_shot;
        let client = match &pooled {
            Some(client) => client,
            None => {
                one_shot = requests::Client::new()?;
                &one_shot
            }
        };
        let start = Instant::now();
        let response = client.get(&url).send().await?;
        let head = start.elapsed();
        let body_start = Instant::now();
        let bytes = response.bytes().await?;
        assert_eq!(bytes.len(), options.body_bytes);
        let body = body_start.elapsed();
        if index < options.warmup {
            continue;
        }
        totals.push(start.elapsed().as_nanos() as u64);
        heads.push(head.as_nanos() as u64);
        bodies.push(body.as_nanos() as u64);
    }
    Ok(Report {
        label: format!(
            "async/{} pool={} body={}B",
            options.runtime, options.mode, options.body_bytes
        ),
        requests: options.requests,
        elapsed: Duration::from_nanos(totals.iter().sum::<u64>()),
        totals,
        heads,
        bodies,
    })
}

fn measure_blocking(options: &Options, port: u16) -> Result<Report, DynError> {
    let url = format!("http://127.0.0.1:{port}/small");
    let pooled = (options.mode == "pooled")
        .then(requests::blocking::Client::new)
        .transpose()?;
    let mut totals = Vec::with_capacity(options.requests);
    let mut heads = Vec::with_capacity(options.requests);
    let mut bodies = Vec::with_capacity(options.requests);
    for index in 0..options.warmup + options.requests {
        let one_shot;
        let client = match &pooled {
            Some(client) => client,
            None => {
                one_shot = requests::blocking::Client::new()?;
                &one_shot
            }
        };
        let start = Instant::now();
        let response = client.get(&url).send()?;
        let head = start.elapsed();
        let body_start = Instant::now();
        let mut body = response.into_body();
        let mut buffer = vec![0_u8; CHUNK];
        let mut total = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total += read;
        }
        assert_eq!(total, options.body_bytes);
        let body = body_start.elapsed();
        if index < options.warmup {
            continue;
        }
        totals.push(start.elapsed().as_nanos() as u64);
        heads.push(head.as_nanos() as u64);
        bodies.push(body.as_nanos() as u64);
    }
    Ok(Report {
        label: format!("blocking pool={} body={}B", options.mode, options.body_bytes),
        requests: options.requests,
        elapsed: Duration::from_nanos(totals.iter().sum::<u64>()),
        totals,
        heads,
        bodies,
    })
}

async fn measure_raw(options: &Options, port: u16) -> Result<Report, DynError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let request = format!(
        "GET /small HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n"
    );
    let mut totals = Vec::with_capacity(options.requests);
    let mut heads = Vec::with_capacity(options.requests);
    let mut bodies = Vec::with_capacity(options.requests);
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    stream.set_nodelay(true)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut pending = Vec::new();
    for index in 0..options.warmup + options.requests {
        let start = Instant::now();
        stream.write_all(request.as_bytes()).await?;
        let head_end;
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err("fixture closed".into());
            }
            pending.extend_from_slice(&buffer[..read]);
            if let Some(position) = pending
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                head_end = position + 4;
                break;
            }
        }
        let head = start.elapsed();
        let body_start = Instant::now();
        let header = String::from_utf8_lossy(&pending[..head_end]).to_ascii_lowercase();
        let length: usize = header
            .split("content-length:")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .and_then(|value| value.trim().parse().ok())
            .ok_or("missing content-length")?;
        while pending.len() - head_end < length {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err("fixture closed mid-body".into());
            }
            pending.extend_from_slice(&buffer[..read]);
        }
        let consumed = head_end + length;
        pending.drain(..consumed);
        let body = body_start.elapsed();
        if index < options.warmup {
            continue;
        }
        totals.push(start.elapsed().as_nanos() as u64);
        heads.push(head.as_nanos() as u64);
        bodies.push(body.as_nanos() as u64);
    }
    Ok(Report {
        label: "raw-tokio keep-alive".to_owned(),
        requests: options.requests,
        elapsed: Duration::from_nanos(totals.iter().sum::<u64>()),
        totals,
        heads,
        bodies,
    })
}

fn main() -> Result<(), DynError> {
    let options = parse_options()?;
    let body = Arc::new(vec![b'x'; options.body_bytes]);
    let port = spawn_fixture(Arc::clone(&body))?;
    let requests = options.requests;
    let wall = Instant::now();
    let report = match options.surface.as_str() {
        "raw" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(measure_raw(&options, port))?
        }
        "blocking" => measure_blocking(&options, port)?,
        _ => {
            let runtime = match options.runtime.as_str() {
                "current" => tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
                _ => tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?,
            };
            runtime.block_on(measure_async(&options, port))?
        }
    };
    let wall = wall.elapsed();
    let fixture_requests = FIXTURE_REQUESTS.load(Ordering::Relaxed);
    report.print(options.body_bytes);
    println!(
        "  wall {:.3}s for {} measured requests ({:.0} req/s wall), fixture served {}",
        wall.as_secs_f64(),
        requests,
        requests as f64 / wall.as_secs_f64(),
        fixture_requests
    );
    Ok(())
}
