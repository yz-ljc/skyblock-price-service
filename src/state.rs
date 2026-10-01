use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

use crate::{
    config::Config,
    model::{AuctionSnapshot, BazaarSnapshot, CatalogSnapshot},
};

#[derive(Default)]
pub struct Counters {
    pub upstream_requests: AtomicU64,
    pub upstream_429: AtomicU64,
    pub sync_successes: AtomicU64,
    pub sync_failures: AtomicU64,
    pub snapshot_mismatches: AtomicU64,
    pub persistence_failures: AtomicU64,
    pub query_rejections: AtomicU64,
    pub queries: AtomicU64,
}

#[derive(Clone, Default)]
pub struct SyncStatus {
    pub source: &'static str,
    pub started_at: u64,
    pub finished_at: u64,
    pub duration_ms: u64,
    pub last_error: Option<String>,
}

pub struct State {
    pub config: Config,
    pub token: String,
    pub catalog: ArcSwap<CatalogSnapshot>,
    pub bazaar: ArcSwap<BazaarSnapshot>,
    pub auctions: ArcSwap<AuctionSnapshot>,
    pub running: Arc<AtomicBool>,
    pub refreshing: AtomicBool,
    pub counters: Counters,
    pub sync: Mutex<SyncStatus>,
    pub query_slots: Arc<Semaphore>,
    pub started: Instant,
}

impl State {
    pub fn new(config: Config, token: String) -> Self {
        let slots = config.max_query_concurrency;
        Self {
            config,
            token,
            catalog: ArcSwap::from_pointee(CatalogSnapshot::default()),
            bazaar: ArcSwap::from_pointee(BazaarSnapshot::default()),
            auctions: ArcSwap::from_pointee(AuctionSnapshot::default()),
            running: Arc::new(AtomicBool::new(true)),
            refreshing: AtomicBool::new(false),
            counters: Counters::default(),
            sync: Mutex::new(SyncStatus::default()),
            query_slots: Arc::new(Semaphore::new(slots)),
            started: Instant::now(),
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
