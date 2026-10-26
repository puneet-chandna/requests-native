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
    surfaces: Vec<String>,
    rounds: usize,
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
        surfaces: Vec::new(),
        rounds: 1,
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
            "--surfaces" => {
                options.surfaces = value()?
                    .split(',')
                    .filter(|surface| !surface.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "--rounds" => options.rounds = value()?.parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    if !matches!(options.mode.as_str(), "pooled" | "one-shot")
        || !matches!(options.surface.as_str(), "async" | "blocking" | "raw" | "hyper")
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

/// Hand-written hyper client over one pooled connection: the minimum a
/// hyper-based client can cost for a keep-alive request, with no pool, no
/// observer and no response translation above it.
async fn measure_hyper(options: &Options, port: u16) -> Result<Report, DynError> {
    use http_body_util::Empty;
    use hyper::Request;
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;

    let host = format!("127.0.0.1:{port}");
    let connect = || async {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        stream.set_nodelay(true)?;
        let (sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        // The connection future must be polled before the first request.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut sender: hyper::client::conn::http1::SendRequest<Empty<Bytes>> = sender;
        sender.ready().await?;
        Ok::<_, DynError>(sender)
    };
    let mut sender = connect().await?;
    let mut reconnects = 0usize;
    let mut totals = Vec::with_capacity(options.requests);
    let mut heads = Vec::with_capacity(options.requests);
    let mut bodies = Vec::with_capacity(options.requests);
    for index in 0..options.warmup + options.requests {
        let request = Request::builder()
            .method("GET")
            .uri("/small")
            .header("Host", host.clone())
            .header("Accept-Encoding", "identity")
            .body(Empty::<Bytes>::new())?;
        let start = Instant::now();
        let response = match sender.send_request(request).await {
            Ok(response) => response,
            Err(_) => {
                // A keep-alive connection can be closed under the baseline by
                // the fixture; reconnect and keep the measurement loop honest.
                reconnects += 1;
                sender = connect().await?;
                continue;
            }
        };
        let head = start.elapsed();
        let body_start = Instant::now();
        let body = http_body_util::BodyExt::collect(response.into_body()).await?;
        assert_eq!(body.to_bytes().len(), options.body_bytes);
        let body = body_start.elapsed();
        if index < options.warmup {
            continue;
        }
        totals.push(start.elapsed().as_nanos() as u64);
        heads.push(head.as_nanos() as u64);
        bodies.push(body.as_nanos() as u64);
    }
    if reconnects > 0 {
        eprintln!("hyper baseline reconnected {reconnects} times");
    }
    Ok(Report {
        label: "hyper http1 conn (pooled, no pool layer)".to_owned(),
        requests: options.requests,
        elapsed: Duration::from_nanos(totals.iter().sum::<u64>()),
        totals,
        heads,
        bodies,
    })
}

fn measure_surface(surface: &str, options: &Options, port: u16) -> Result<Report, DynError> {
    match surface {
        "raw" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(measure_raw(options, port))
        }
        "hyper" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(measure_hyper(options, port))
        }
        "blocking" => measure_blocking(options, port),
        _ => {
            let runtime = match options.runtime.as_str() {
                "current" => tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
                _ => tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?,
            };
            runtime.block_on(measure_async(options, port))
        }
    }
}

/// Interleaved rounds of every requested surface.
///
/// Alternating surfaces inside one process round makes each round a paired
/// comparison, so a drifting machine affects every surface of that round
/// instead of only whichever surface ran while it was quiet.
fn run_rounds(options: &Options, port: u16) -> Result<(), DynError> {
    let mut medians: Vec<(String, Vec<f64>, Vec<f64>)> = Vec::new();
    for surface in &options.surfaces {
        medians.push((surface.clone(), Vec::new(), Vec::new()));
    }
    for round in 0..options.rounds {
        for (index, surface) in options.surfaces.iter().enumerate() {
            let report = measure_surface(surface, options, port)?;
            let mut totals = report.totals.clone();
            totals.sort_unstable();
            let p50 = percentile(&totals, 0.50);
            let mut heads = report.heads.clone();
            heads.sort_unstable();
            let mut bodies = report.bodies.clone();
            bodies.sort_unstable();
            let head = percentile(&heads, 0.50);
            let body = percentile(&bodies, 0.50);
            let elapsed = report.elapsed.as_secs_f64();
            let rate = report.requests as f64 / elapsed;
            println!(
                "round {round} {:<9} p50={:>7.1}us (head {:>6.1} body {:>5.1})  {:>9.0} req/s",
                surface, p50, head, body, rate
            );
            medians[index].1.push(p50);
            medians[index].2.push(rate);
        }
    }
    println!();
    println!("surface    median p50   median req/s   vs first");
    let baseline_p50 = median(&medians[0].1);
    for (name, p50s, rates) in &medians {
        let p50 = median(p50s);
        println!(
            "{:<10} {:>7.1}us {:>11.0} {:>10.2}x",
            name, p50,
            median(rates),
            p50 / baseline_p50
        );
    }
    Ok(())
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    sorted[sorted.len() / 2]
}

fn main() -> Result<(), DynError> {
    let options = parse_options()?;
    let body = Arc::new(vec![b'x'; options.body_bytes]);
    let port = spawn_fixture(Arc::clone(&body))?;
    if !options.surfaces.is_empty() {
        return run_rounds(&options, port);
    }
    let requests = options.requests;
    let wall = Instant::now();
    let report = measure_surface(&options.surface, &options, port)?;
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
