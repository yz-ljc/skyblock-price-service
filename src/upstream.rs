use std::{
    io::{self, BufReader, Read},
    sync::{Arc, Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
};
use serde::de::DeserializeOwned;
use tracing::{info, warn};

use crate::{
    json_stream::{self, Metadata},
    model::{Text, now_ms},
    state::State,
};

#[derive(Clone)]
pub struct Upstream {
    client: Client,
    state: Arc<State>,
    base: reqwest::Url,
    gate: Arc<Mutex<RequestGate>>,
}

struct RequestGate {
    last_request: Option<Instant>,
    cooldown_until: Instant,
}

pub struct RecordSet {
    pub field: &'static str,
    pub object: bool,
    pub limit: usize,
}

impl Upstream {
    /// Construct and use only on the dedicated sync thread, outside Tokio's async runtime.
    pub fn new(state: Arc<State>) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(state.config.request_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(state.config.auction_page_concurrency)
            .user_agent(concat!(
                "AtriMeow-SkyBlock-Prices/",
                env!("CARGO_PKG_VERSION")
            ))
            .gzip(true)
            .build()?;
        let base = reqwest::Url::parse(&format!(
            "{}/",
            state.config.hypixel_base_url.trim_end_matches('/')
        ))?;
        Ok(Self {
            client,
            state,
            base,
            gate: Arc::new(Mutex::new(RequestGate {
                last_request: None,
                cooldown_until: Instant::now(),
            })),
        })
    }

    pub fn parse<T, F>(
        &mut self,
        path: &str,
        records: RecordSet,
        skip_updated: Option<u64>,
        deadline: Instant,
        consume: F,
    ) -> Result<Metadata>
    where
        T: DeserializeOwned,
        F: FnMut(Option<Text<128>>, T) -> Result<()>,
    {
        let started = Instant::now();
        let page_deadline =
            deadline.min(started + Duration::from_secs(self.state.config.response_timeout_secs));
        let response = self.response(path, page_deadline).with_context(|| {
            format!(
                "request {path} failed after {}ms",
                started.elapsed().as_millis()
            )
        })?;
        let budget = BudgetReader {
            inner: response,
            remaining: self.state.config.max_response_bytes,
            deadline: page_deadline,
            running: Some(self.state.running.clone()),
        };
        let mut reader = BufReader::with_capacity(16384, budget);
        let parsed = json_stream::parse(
            &mut reader,
            records.field,
            records.object,
            records.limit,
            skip_updated,
            consume,
        );
        let bytes = self.state.config.max_response_bytes - reader.get_ref().remaining;
        let elapsed = started.elapsed().as_millis();
        let metadata = parsed.with_context(|| {
            format!(
                "reading {path}: elapsed={elapsed}ms, decoded_bytes={bytes}, page_budget={}s",
                self.state.config.response_timeout_secs
            )
        })?;
        ensure!(
            Instant::now() < page_deadline,
            "page deadline exceeded: {path}, elapsed={elapsed}ms, decoded_bytes={bytes}"
        );
        if elapsed >= 5000 {
            info!(
                path,
                duration_ms = elapsed as u64,
                decoded_bytes = bytes,
                "slow upstream page completed"
            );
        }
        ensure!(
            metadata.last_updated.is_some_and(|v| v <= now_ms() + 60000),
            "upstream timestamp is in the future"
        );
        Ok(metadata)
    }

    fn response(&mut self, path: &str, deadline: Instant) -> Result<Response> {
        let url = self.base.join(path)?;
        for attempt in 0..=self.state.config.request_retries {
            self.request_slot(deadline)?;
            self.state
                .counters
                .upstream_requests
                .fetch_add(1, Ordering::Relaxed);
            // Keep the client's blocking read timeout. A per-request timeout also
            // imposes an async whole-body deadline and would cut slow pages short.
            match self.client.get(url.clone()).send() {
                Ok(response) if response.status().is_success() => {
                    ensure!(
                        response
                            .content_length()
                            .is_none_or(|v| v <= self.state.config.max_response_bytes),
                        "upstream response too large"
                    );
                    return Ok(response);
                }
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        self.state
                            .counters
                            .upstream_429
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                        bail!("upstream HTTP {status}");
                    }
                    let delay = retry_delay(&response)
                        .unwrap_or(Duration::from_secs(2u64.pow(attempt + 1)));
                    warn!(
                        path,
                        status = status.as_u16(),
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "upstream HTTP retry scheduled"
                    );
                    self.cooldown(delay);
                    if attempt == self.state.config.request_retries
                        || delay.as_secs() > self.state.config.max_retry_delay_secs
                    {
                        bail!("upstream HTTP {status}; retry deferred");
                    }
                    // Honor Retry-After across all sources, even if this round is abandoned.
                    drop(response);
                }
                Err(error) => {
                    warn!(path, attempt = attempt + 1, error = %error, "upstream request attempt failed");
                    if attempt == self.state.config.request_retries {
                        return Err(error).context("upstream request failed");
                    }
                    self.cooldown(Duration::from_secs(2u64.pow(attempt + 1)));
                }
            }
        }
        unreachable!()
    }

    // All page workers and sources share actual request-start pacing and server cooldowns.
    fn request_slot(&self, deadline: Instant) -> Result<()> {
        loop {
            ensure!(
                self.state.running.load(Ordering::Relaxed),
                "shutdown requested"
            );
            let mut gate = self.gate.lock().unwrap_or_else(|v| v.into_inner());
            let now = Instant::now();
            ensure!(now < deadline, "sync deadline exceeded");
            let until = gate.last_request.map_or(gate.cooldown_until, |previous| {
                gate.cooldown_until
                    .max(previous + Duration::from_millis(self.state.config.request_spacing_ms))
            });
            if now >= until {
                gate.last_request = Some(now);
                return Ok(());
            }
            drop(gate);
            wait(&self.state, until, deadline)?;
        }
    }

    fn cooldown(&self, delay: Duration) {
        let mut gate = self.gate.lock().unwrap_or_else(|v| v.into_inner());
        gate.cooldown_until = gate.cooldown_until.max(Instant::now() + delay);
    }
}

fn retry_delay(response: &Response) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(86400)));
    }
    Some(
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            .min(Duration::from_secs(86400)),
    )
}

pub fn wait(state: &State, until: Instant, deadline: Instant) -> Result<()> {
    loop {
        ensure!(state.running.load(Ordering::Relaxed), "shutdown requested");
        let now = Instant::now();
        ensure!(now < deadline, "sync deadline exceeded");
        if now >= until {
            return Ok(());
        }
        thread::sleep((until - now).min(Duration::from_millis(100)));
    }
}

/// Budgets count decompressed HTTP body bytes, and also apply to on-disk checkpoint reads.
pub struct BudgetReader<R> {
    pub inner: R,
    pub remaining: u64,
    pub deadline: Instant,
    pub running: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if Instant::now() >= self.deadline
            || self
                .running
                .as_ref()
                .is_some_and(|v| !v.load(Ordering::Relaxed))
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "read cancelled or timed out",
            ));
        }
        if self.remaining == 0 {
            let mut extra = [0];
            return if self.inner.read(&mut extra)? == 0 {
                Ok(0)
            } else {
                Err(io::Error::other("byte budget exceeded"))
            };
        }
        let size = buffer
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut buffer[..size])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}
