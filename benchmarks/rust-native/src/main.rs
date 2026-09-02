#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::future::poll_fn;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_core::Stream;

type DynError = Box<dyn Error + Send + Sync>;
type Observation = (u64, usize, u64, u64);

#[derive(Clone)]
struct Config {
    surface: String,
    url: String,
    requests: usize,
    concurrency: usize,
    mode: String,
    read: String,
    chunk_size: usize,
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
    while let Some(flag) = arguments.next() {
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
    let (size, digest) = if config.read == "buffered" {
        let body = response.bytes().await?;
        (body.len(), checksum(&body))
    } else {
        let mut body = response.into_body();
        let mut size = 0;
        let mut digest = 0;
        while let Some(chunk) = poll_fn(|context| Pin::new(&mut body).poll_next(context)).await {
            let chunk = chunk?;
            size += chunk.len();
            digest += checksum(&chunk);
        }
        (size, digest)
    };
    Ok((
        started.elapsed().as_nanos() as u64,
        size,
        digest,
        connection,
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
    let (size, digest) = if config.read == "buffered" {
        let body = response.bytes()?;
        (body.len(), checksum(&body))
    } else {
        let mut body = response.into_body();
        let mut buffer = vec![0; config.chunk_size];
        let mut size = 0;
        let mut digest = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read;
            digest += checksum(&buffer[..read]);
        }
        (size, digest)
    };
    Ok((
        started.elapsed().as_nanos() as u64,
        size,
        digest,
        connection,
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
    let cpu_started = cpu_seconds();
    let started = Instant::now();
    let observations = if config.surface == "rust-async" {
        run_async(&config).await?
    } else {
        run_blocking(&config)?
    };
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
        "{{\"latencies_ns\":[{latencies}],\"body_bytes\":{body_bytes},\"checksum\":{digest},\"connection_ids\":[{connection_ids}],\"elapsed_ns\":{elapsed},\"cpu_seconds\":{},\"rss_peak_bytes\":{},\"python_peak_alloc_bytes\":null,\"native_responses\":{},\"implementation\":\"requests-native\"}}",
        json_number(cpu),
        json_integer(rss_peak_bytes()),
        config.requests,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_work;

    #[test]
    fn split_work_preserves_all_requests() {
        assert_eq!(split_work(11, 4), [3, 3, 3, 2]);
    }
}
