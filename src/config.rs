use std::{env, fs, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind: SocketAddr,
    pub data_directory: PathBuf,
    pub hypixel_base_url: String,
    pub request_timeout_secs: u64,
    pub response_timeout_secs: u64,
    pub request_spacing_ms: u64,
    pub request_retries: u32,
    pub max_retry_delay_secs: u64,
    pub catalog_interval_secs: u64,
    pub bazaar_interval_secs: u64,
    pub auction_interval_secs: u64,
    pub auction_page_concurrency: usize,
    pub round_timeout_secs: u64,
    pub stale_after_secs: u64,
    pub max_price_age_secs: u64,
    pub max_restore_age_secs: u64,
    pub max_response_bytes: u64,
    pub max_snapshot_bytes: u64,
    pub max_catalog_items: usize,
    pub max_bazaar_products: usize,
    pub max_auction_pages: usize,
    pub max_auctions: usize,
    pub max_price_entries: usize,
    pub max_nbt_encoded_bytes: usize,
    pub max_nbt_decoded_bytes: usize,
    pub max_query_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25577".parse().unwrap(),
            data_directory: "data".into(),
            hypixel_base_url: "https://api.hypixel.net/".into(),
            request_timeout_secs: 15,
            response_timeout_secs: 60,
            request_spacing_ms: 500,
            request_retries: 2,
            max_retry_delay_secs: 60,
            catalog_interval_secs: 21600,
            bazaar_interval_secs: 60,
            auction_interval_secs: 90,
            auction_page_concurrency: 4,
            round_timeout_secs: 180,
            stale_after_secs: 300,
            max_price_age_secs: 3600,
            max_restore_age_secs: 86400,
            max_response_bytes: 16 * 1024 * 1024,
            max_snapshot_bytes: 16 * 1024 * 1024,
            max_catalog_items: 15000,
            max_bazaar_products: 10000,
            max_auction_pages: 200,
            max_auctions: 200000,
            max_price_entries: 50000,
            max_nbt_encoded_bytes: 65536,
            max_nbt_decoded_bytes: 262144,
            max_query_concurrency: 16,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = env::var_os("PRICE_CONFIG")
            .map(PathBuf::from)
            .unwrap_or("config.toml".into());
        let mut config: Self = if path.exists() {
            ensure!(
                fs::metadata(&path)?.len() <= 65536,
                "configuration is too large"
            );
            toml::from_str(&fs::read_to_string(&path)?).context("invalid configuration")?
        } else if env::var_os("PRICE_CONFIG").is_some() {
            anyhow::bail!("PRICE_CONFIG does not exist")
        } else {
            Self::default()
        };
        if let Ok(value) = env::var("PRICE_BIND") {
            config.bind = value.parse().context("invalid PRICE_BIND")?;
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8).contains(&self.auction_page_concurrency),
            "invalid auction page concurrency"
        );
        let url = reqwest::Url::parse(&self.hypixel_base_url)?;
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
            "upstream must use HTTPS, except loopback development fixtures"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "upstream URL cannot contain credentials, query or fragment"
        );
        ensure!(
            (1..=120).contains(&self.request_timeout_secs),
            "invalid request timeout"
        );
        ensure!(
            (1..=300).contains(&self.response_timeout_secs),
            "invalid response timeout"
        );
        ensure!(
            (250..=60000).contains(&self.request_spacing_ms),
            "invalid request spacing"
        );
        ensure!(
            self.request_retries <= 3 && (1..=120).contains(&self.max_retry_delay_secs),
            "invalid retry limits"
        );
        ensure!(
            [
                self.catalog_interval_secs,
                self.bazaar_interval_secs,
                self.auction_interval_secs,
                self.round_timeout_secs,
                self.stale_after_secs
            ]
            .iter()
            .all(|v| *v >= 1),
            "intervals must be positive"
        );
        ensure!(
            self.max_price_age_secs >= self.stale_after_secs
                && self.max_restore_age_secs >= self.max_price_age_secs,
            "invalid snapshot age limits"
        );
        ensure!(
            (65536..=64 * 1024 * 1024).contains(&self.max_response_bytes),
            "invalid response byte budget"
        );
        ensure!(
            (65536..=32 * 1024 * 1024).contains(&self.max_snapshot_bytes),
            "invalid snapshot byte budget"
        );
        ensure!(
            (1..=50000).contains(&self.max_catalog_items)
                && (1..=20000).contains(&self.max_bazaar_products),
            "invalid catalog limits"
        );
        ensure!(
            (1..=500).contains(&self.max_auction_pages)
                && (1..=500000).contains(&self.max_auctions),
            "invalid auction limits"
        );
        ensure!(
            (1..=100000).contains(&self.max_price_entries),
            "invalid price entry limit"
        );
        ensure!(
            (1024..=262144).contains(&self.max_nbt_encoded_bytes)
                && (1024..=1048576).contains(&self.max_nbt_decoded_bytes),
            "invalid NBT budgets"
        );
        ensure!(
            (1..=64).contains(&self.max_query_concurrency),
            "invalid query concurrency"
        );
        Ok(())
    }
}
