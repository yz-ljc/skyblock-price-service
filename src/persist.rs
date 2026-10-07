use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

use crate::{
    config::Config,
    model::{AuctionSnapshot, BazaarSnapshot, CatalogSnapshot, normalized, now_ms},
    state::State,
    upstream::BudgetReader,
};

#[derive(Deserialize, Serialize)]
struct Checkpoint<T> {
    schema: u32,
    data: T,
}

pub fn lock_directory(config: &Config) -> Result<File> {
    fs::create_dir_all(&config.data_directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.data_directory.join("service.lock"))?;
    file.try_lock_exclusive()?;
    Ok(file)
}

/// Checkpoint replacement is atomic on the same filesystem; keep at most one file per source.
pub fn save<T: Serialize>(config: &Config, source: &str, data: &T) -> Result<()> {
    let target = config.data_directory.join(format!("{source}.json"));
    let temporary = config.data_directory.join(format!("{source}.json.tmp"));
    let result = (|| {
        let file = File::create(&temporary)?;
        let mut writer = BudgetWriter {
            inner: BufWriter::new(file),
            remaining: config.max_snapshot_bytes,
        };
        serde_json::to_writer(&mut writer, &Checkpoint { schema: 1, data })?;
        writer.flush()?;
        writer.inner.get_ref().sync_all()?;
        drop(writer);
        fs::rename(&temporary, &target)?;
        #[cfg(unix)]
        File::open(&config.data_directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load<T: DeserializeOwned>(config: &Config, path: &Path) -> Result<T> {
    ensure!(
        fs::metadata(path)?.len() <= config.max_snapshot_bytes,
        "checkpoint too large"
    );
    let reader = BudgetReader {
        inner: File::open(path)?,
        remaining: config.max_snapshot_bytes,
        deadline: Instant::now() + Duration::from_secs(10),
        running: None,
    };
    let checkpoint: Checkpoint<T> = serde_json::from_reader(BufReader::new(reader))?;
    ensure!(checkpoint.schema == 1, "unsupported checkpoint schema");
    Ok(checkpoint.data)
}

fn valid_time(updated: u64, fetched: u64, config: &Config) -> Result<()> {
    let now = now_ms();
    ensure!(
        updated > 0 && fetched > 0 && updated <= now + 60000 && fetched <= now + 60000,
        "invalid checkpoint timestamp"
    );
    ensure!(
        now.saturating_sub(fetched) / 1000 <= config.max_restore_age_secs,
        "checkpoint is too old"
    );
    Ok(())
}

pub fn restore(state: &State) {
    for source in ["catalog", "bazaar", "auctions"] {
        let path = state.config.data_directory.join(format!("{source}.json"));
        if !path.is_file() {
            continue;
        }
        let result = match source {
            "catalog" => restore_catalog(state, &path),
            "bazaar" => restore_bazaar(state, &path),
            _ => restore_auctions(state, &path),
        };
        if let Err(error) = result {
            warn!(source, error = %error, "checkpoint not restored");
        }
    }
}

fn restore_catalog(state: &State, path: &Path) -> Result<()> {
    let mut data: CatalogSnapshot = load(&state.config, path)?;
    valid_time(data.last_updated, data.fetched_at, &state.config)?;
    ensure!(
        !data.items.is_empty() && data.items.len() <= state.config.max_catalog_items,
        "invalid catalog count"
    );
    for (key, item) in &mut data.items {
        crate::nbt::validate_id(&key.0)?;
        ensure!(key == &item.id, "catalog key mismatch");
        ensure!(
            item.npc_sell_price
                .is_none_or(|v| v.is_finite() && (0.0..=1e16).contains(&v)),
            "invalid NPC price"
        );
        item.search = normalized(&format!("{} {}", item.id.0, item.name.0));
    }
    state.catalog.store(std::sync::Arc::new(data));
    Ok(())
}

fn restore_bazaar(state: &State, path: &Path) -> Result<()> {
    let data: BazaarSnapshot = load(&state.config, path)?;
    valid_time(data.last_updated, data.fetched_at, &state.config)?;
    ensure!(
        !data.products.is_empty() && data.products.len() <= state.config.max_bazaar_products,
        "invalid bazaar count"
    );
    for (id, quote) in &data.products {
        crate::nbt::validate_id(&id.0)?;
        ensure!(
            [
                quote.instant_buy,
                quote.instant_sell,
                quote.buy_order,
                quote.sell_offer
            ]
            .iter()
            .flatten()
            .all(|v| crate::model::valid_price(*v).is_some()),
            "invalid bazaar price"
        );
    }
    state.bazaar.store(std::sync::Arc::new(data));
    Ok(())
}

fn restore_auctions(state: &State, path: &Path) -> Result<()> {
    let mut data: AuctionSnapshot = load(&state.config, path)?;
    valid_time(data.last_updated, data.fetched_at, &state.config)?;
    ensure!(
        data.items.len() <= state.config.max_price_entries
            && data.total_auctions <= state.config.max_auctions,
        "invalid auction count"
    );
    for (key, item) in &mut data.items {
        crate::nbt::validate_id(&item.id.0)?;
        ensure!(
            item.variant.enchantments.len() <= crate::model::MAX_BOOK_ENCHANTMENTS
                && key.0 == item.variant.key(&item.id.0),
            "invalid variant key"
        );
        ensure!(
            crate::model::valid_price(item.price.lowest_bin).is_some()
                && item.price.quantity > 0
                && item.price.quantity <= 127,
            "invalid auction price"
        );
        item.search = normalized(&format!("{} {} {}", item.id.0, item.name.0, key.0));
    }
    state.auctions.store(std::sync::Arc::new(data));
    Ok(())
}

struct BudgetWriter<W> {
    inner: W,
    remaining: u64,
}
impl<W: Write> Write for BudgetWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::other("checkpoint byte budget exceeded"));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
