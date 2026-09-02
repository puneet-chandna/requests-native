use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeSet;
use std::error::Error;
use std::future::poll_fn;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use futures_core::Stream;

type DynError = Box<dyn Error + Send + Sync>;
type Observation = (u64, usize, u64, u64, usize);

struct CountingAllocator;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every operation delegates to `System` with the original pointer and
// layout. The side counter observes successful allocations without changing
// allocator semantics.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegated with the caller-provided valid layout.
        let pointer = unsafe { System.alloc(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegated with the caller-provided valid layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        record_allocation(pointer, layout.size());
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: delegated with the original pointer and layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: delegated with the original pointer/layout and requested size.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        record_allocation(replacement, new_size);
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_allocation(pointer: *mut u8, bytes: usize) {
    if !pointer.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
        ALLOCATED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

fn begin_allocation_measurement() {
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNT_ALLOCATIONS.store(true, Ordering::Release);
}

fn end_allocation_measurement() -> u64 {
    COUNT_ALLOCATIONS.store(false, Ordering::Release);
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

struct ApplicationChunker {
    chunk_size: usize,
    pending: Vec<u8>,
}

impl ApplicationChunker {
    fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        while self.pending.len() >= self.chunk_size {
            chunks.push(self.pending.drain(..self.chunk_size).collect());
        }
        chunks
    }

    fn finish(self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then_some(self.pending)
    }
}

#[derive(Clone)]
struct Config {
    surface: String,
    url: String,
    requests: usize,
    concurrency: usize,
    mode: String,
    read: String,
    chunk_size: usize,
    measure_allocations: bool,
}

fn parse_args() -> Result<Config, DynError> {
    let mut arguments = std::env::args().skip(1);
    let mut surface = None;
    let mut url = None;
    let mut requests = None;
    let mut concurrency = None;
    let mut mode = None;
    let mut read = None;
    let mut chunk_size = None;
    let mut measure_allocations = false;
    while let Some(flag) = arguments.next() {
        if flag == "--measure-allocations" {
            measure_allocations = true;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--surface" => surface = Some(value),
            "--url" => url = Some(value),
            "--requests" => requests = Some(value.parse()?),
            "--concurrency" => concurrency = Some(value.parse()?),
            "--mode" => mode = Some(value),
            "--read" => read = Some(value),
            "--chunk-size" => chunk_size = Some(value.parse()?),
            _ => return Err(format!("unknown argument: {flag}").into()),
        }
    }
    let config = Config {
        surface: surface.ok_or("missing --surface")?,
        url: url.ok_or("missing --url")?,
        requests: requests.ok_or("missing --requests")?,
        concurrency: concurrency.ok_or("missing --concurrency")?,
        mode: mode.ok_or("missing --mode")?,
        read: read.ok_or("missing --read")?,
        chunk_size: chunk_size.ok_or("missing --chunk-size")?,
        measure_allocations,
    };
    if !matches!(config.surface.as_str(), "rust-async" | "rust-blocking")
        || !matches!(config.mode.as_str(), "one-shot" | "pooled")
        || !matches!(config.read.as_str(), "buffered" | "streaming")
        || config.requests == 0
        || config.concurrency == 0
        || config.chunk_size == 0
    {
        return Err("invalid benchmark configuration".into());
    }
    Ok(config)
}

fn split_work(requests: usize, concurrency: usize) -> Vec<usize> {
    let workers = requests.min(concurrency);
    let quotient = requests / workers;
    let remainder = requests % workers;
    (0..workers)
        .map(|index| quotient + usize::from(index < remainder))
        .collect()
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().map(|byte| u64::from(*byte)).sum()
}

fn connection_id(headers: &requests::HeaderMap) -> Result<u64, DynError> {
    Ok(headers
        .get("x-benchmark-connection")
        .ok_or("fixture response omitted connection id")?
        .to_str()?
        .parse()?)
}

async fn async_request(
    client: &requests::Client,
    config: &Config,
) -> Result<Observation, DynError> {
    let started = Instant::now();
    let response = client.get(&config.url).send().await?;
    let connection = connection_id(response.headers())?;
    let (size, digest, application_chunks) = if config.read == "buffered" {
        let body = response.bytes().await?;
        (body.len(), checksum(&body), 0)
    } else {
        let mut body = response.into_body();
        let mut chunker = ApplicationChunker::new(config.chunk_size);
        let mut size = 0;
        let mut digest = 0;
        let mut application_chunks = 0;
        while let Some(chunk) = poll_fn(|context| Pin::new(&mut body).poll_next(context)).await {
            let chunk = chunk?;
            for application_chunk in chunker.push(&chunk) {
                size += application_chunk.len();
                digest += checksum(&application_chunk);
                application_chunks += 1;
            }
        }
        if let Some(application_chunk) = chunker.finish() {
            size += application_chunk.len();
            digest += checksum(&application_chunk);
            application_chunks += 1;
        }
        (size, digest, application_chunks)
    };
    Ok((
        started.elapsed().as_nanos() as u64,
        size,
        digest,
        connection,
        application_chunks,
    ))
}

async fn async_client(config: Config, count: usize) -> Result<Vec<Observation>, DynError> {
    let pooled = (config.mode == "pooled")
        .then(requests::Client::new)
        .transpose()?;
    let mut observations = Vec::with_capacity(count);
    for _ in 0..count {
        let one_shot;
        let client = if let Some(client) = &pooled {
            client
        } else {
            one_shot = requests::Client::new()?;
            &one_shot
        };
        observations.push(async_request(client, &config).await?);
    }
    Ok(observations)
}

async fn run_async(config: &Config) -> Result<Vec<Observation>, DynError> {
    let mut tasks = tokio::task::JoinSet::new();
    for count in split_work(config.requests, config.concurrency) {
        tasks.spawn(async_client(config.clone(), count));
    }
    let mut observations = Vec::with_capacity(config.requests);
    while let Some(result) = tasks.join_next().await {
        observations.extend(result??);
    }
    Ok(observations)
}

fn blocking_request(
    client: &requests::blocking::Client,
    config: &Config,
) -> Result<Observation, DynError> {
    let started = Instant::now();
    let response = client.get(&config.url).send()?;
    let connection = connection_id(response.headers())?;
    let (size, digest, application_chunks) = if config.read == "buffered" {
        let body = response.bytes()?;
        (body.len(), checksum(&body), 0)
    } else {
        let mut body = response.into_body();
        let mut chunker = ApplicationChunker::new(config.chunk_size);
        let mut buffer = vec![0; 8 * 1024];
        let mut size = 0;
        let mut digest = 0;
        let mut application_chunks = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            for application_chunk in chunker.push(&buffer[..read]) {
                size += application_chunk.len();
                digest += checksum(&application_chunk);
                application_chunks += 1;
            }
        }
        if let Some(application_chunk) = chunker.finish() {
            size += application_chunk.len();
            digest += checksum(&application_chunk);
            application_chunks += 1;
        }
        (size, digest, application_chunks)
    };
    Ok((
        started.elapsed().as_nanos() as u64,
        size,
        digest,
        connection,
        application_chunks,
    ))
}

fn blocking_client(config: &Config, count: usize) -> Result<Vec<Observation>, DynError> {
    let pooled = (config.mode == "pooled")
        .then(requests::blocking::Client::new)
        .transpose()?;
    let mut observations = Vec::with_capacity(count);
    for _ in 0..count {
        let one_shot;
        let client = if let Some(client) = &pooled {
            client
        } else {
            one_shot = requests::blocking::Client::new()?;
            &one_shot
        };
        observations.push(blocking_request(client, config)?);
    }
    Ok(observations)
}

fn run_blocking(config: &Config) -> Result<Vec<Observation>, DynError> {
    let config = Arc::new(config.clone());
    let handles = split_work(config.requests, config.concurrency)
        .into_iter()
        .map(|count| {
            let config = Arc::clone(&config);
            std::thread::spawn(move || blocking_client(&config, count))
        })
        .collect::<Vec<_>>();
    let mut observations = Vec::with_capacity(config.requests);
    for handle in handles {
        observations.extend(handle.join().map_err(|_| "blocking worker panicked")??);
    }
    Ok(observations)
}

#[cfg(target_os = "linux")]
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let ticks = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    let ticks_per_second = std::env::var("BENCH_CLK_TCK")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(100.0);
    Some(ticks / ticks_per_second)
}

#[cfg(not(target_os = "linux"))]
fn cpu_seconds() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn rss_peak_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kibibytes| kibibytes * 1024)
}

#[cfg(not(target_os = "linux"))]
fn rss_peak_bytes() -> Option<u64> {
    None
}

fn json_number(value: Option<f64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

fn json_integer(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), DynError> {
    let config = parse_args()?;
    if config.measure_allocations {
        begin_allocation_measurement();
    }
    let cpu_started = cpu_seconds();
    let started = Instant::now();
    let observations = if config.surface == "rust-async" {
        run_async(&config).await?
    } else {
        run_blocking(&config)?
    };
    let native_total_allocated_bytes = config.measure_allocations.then(end_allocation_measurement);
    let elapsed = started.elapsed().as_nanos();
    let cpu = cpu_seconds()
        .zip(cpu_started)
        .map(|(end, start)| end - start);
    let body_bytes = observations.iter().map(|item| item.1).sum::<usize>();
    let digest = observations.iter().map(|item| item.2).sum::<u64>();
    let connections = observations
        .iter()
        .map(|item| item.3)
        .collect::<BTreeSet<_>>();
    let application_chunks = observations.iter().map(|item| item.4).sum::<usize>();
    let latencies = observations
        .iter()
        .map(|item| item.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let connection_ids = connections
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"latencies_ns\":[{latencies}],\"body_bytes\":{body_bytes},\"checksum\":{digest},\"connection_ids\":[{connection_ids}],\"elapsed_ns\":{elapsed},\"cpu_seconds\":{},\"rss_peak_bytes\":{},\"python_peak_alloc_bytes\":null,\"native_total_allocated_bytes\":{},\"native_responses\":{},\"application_chunks\":{application_chunks},\"implementation\":\"requests-native\"}}",
        json_number(cpu),
        json_integer(rss_peak_bytes()),
        json_integer(native_total_allocated_bytes),
        config.requests,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationChunker, begin_allocation_measurement, end_allocation_measurement, split_work,
    };

    #[test]
    fn split_work_preserves_all_requests() {
        assert_eq!(split_work(11, 4), [3, 3, 3, 2]);
    }

    #[test]
    fn application_chunks_ignore_transport_frame_boundaries_and_chunk_size() {
        for (chunk_size, expected) in [(1, vec![1; 14]), (4, vec![4, 4, 4, 2]), (7, vec![7, 7])] {
            let mut chunker = ApplicationChunker::new(chunk_size);
            let mut lengths = Vec::new();
            for frame in [b"abc".as_slice(), b"defgh", b"i", b"jklmn"] {
                lengths.extend(chunker.push(frame).iter().map(Vec::len));
            }
            if let Some(remainder) = chunker.finish() {
                lengths.push(remainder.len());
            }
            assert_eq!(lengths, expected);
        }
    }

    #[test]
    fn allocation_replay_counts_successful_system_allocations() {
        begin_allocation_measurement();
        let allocation = std::hint::black_box(vec![0_u8; 1024]);
        let allocated = end_allocation_measurement();
        assert_eq!(allocation.len(), 1024);
        assert!(allocated >= 1024);
    }
}
