use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
};
use tracing::{error, info, warn};

use crate::{
    json_stream::Metadata,
    model::{
        AuctionItem, AuctionQuote, AuctionSnapshot, BazaarQuote, BazaarSnapshot, CatalogSnapshot,
        Icon, Item, Text, normalized, now_ms, plain_name, valid_price,
    },
    nbt, persist,
    state::{State, SyncStatus},
    upstream::{RecordSet, Upstream, wait},
};

#[derive(Deserialize)]
struct RawSkin {
    value: Text<8192>,
}

#[derive(Deserialize)]
struct RawItem {
    id: Text<128>,
    name: Text<256>,
    material: Text<96>,
    tier: Option<Text<32>>,
    durability: Option<u32>,
    color: Option<Text<32>>,
    item_model: Option<Text<256>>,
    skin: Option<RawSkin>,
    npc_sell_price: Option<f64>,
    #[serde(default)]
    glowing: bool,
}

#[derive(Default)]
struct Orders(Vec<Order>);
impl<'de> Deserialize<'de> for Orders {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct OrderVisitor;
        impl<'de> Visitor<'de> for OrderVisitor {
            type Value = Orders;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("at most 128 bazaar summary rows")
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Orders, S::Error> {
                let mut orders = Vec::new();
                while let Some(order) = seq.next_element()? {
                    if orders.len() >= 128 {
                        return Err(de::Error::custom("bazaar summary limit exceeded"));
                    }
                    orders.push(order);
                }
                Ok(Orders(orders))
            }
        }
        d.deserialize_seq(OrderVisitor)
    }
}

#[derive(Deserialize)]
struct Order {
    #[serde(rename = "pricePerUnit")]
    price: f64,
    amount: f64,
}

#[derive(Deserialize)]
struct QuickStatus {
    #[serde(rename = "buyVolume")]
    buy_volume: f64,
    #[serde(rename = "sellVolume")]
    sell_volume: f64,
}

#[derive(Deserialize)]
struct RawProduct {
    product_id: Text<128>,
    buy_summary: Orders,
    sell_summary: Orders,
    quick_status: QuickStatus,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ItemBytes {
    String(Text<262144>),
    Object { data: Text<262144> },
}
impl ItemBytes {
    fn data(&self) -> &str {
        match self {
            Self::String(v) | Self::Object { data: v } => &v.0,
        }
    }
}

#[derive(Deserialize)]
struct RawAuction {
    uuid: Text<64>,
    item_name: Text<256>,
    tier: Option<Text<32>>,
    starting_bid: u64,
    end: u64,
    item_bytes: ItemBytes,
    #[serde(default)]
    bin: bool,
    #[serde(default)]
    claimed: bool,
}

#[derive(Debug)]
struct AuctionPending {
    generation: u64,
    received: usize,
    total: usize,
    missing: Vec<usize>,
    retry_after_secs: u64,
}

impl fmt::Display for AuctionPending {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "auction generation {} has {}/{} pages; missing pages (first 16): {:?}",
            self.generation, self.received, self.total, self.missing
        )
    }
}

impl std::error::Error for AuctionPending {}

struct AuctionGeneration {
    identity: (u64, usize, usize),
    pages: Vec<bool>,
    count: usize,
    value: AuctionSnapshot,
}

#[derive(Default)]
struct AuctionAssembly {
    // CDN pages can lag several generations. Bound both generation count and total entries.
    generations: BTreeMap<u64, AuctionGeneration>,
    next_page: usize,
}

impl AuctionAssembly {
    fn prune(&mut self, state: &State) {
        let published = state.auctions.load().last_updated;
        let oldest = now_ms().saturating_sub(state.config.max_price_age_secs.saturating_mul(1000));
        self.generations
            .retain(|updated, _| *updated > published && *updated >= oldest);
    }

    fn accept(
        &mut self,
        state: &State,
        page: usize,
        meta: Metadata,
        part: AuctionSnapshot,
    ) -> Result<()> {
        let identity = validate_page(state, &meta, page)?;
        let published = state.auctions.load().last_updated;
        if identity.0 <= published {
            return Ok(());
        }
        if !self.generations.contains_key(&identity.0) && self.generations.len() == 6 {
            let oldest = *self.generations.first_key_value().unwrap().0;
            if identity.0 < oldest {
                return Ok(());
            }
            self.generations.pop_first();
        }
        let generation = self
            .generations
            .entry(identity.0)
            .or_insert_with(|| AuctionGeneration {
                identity,
                pages: vec![false; identity.1],
                count: 0,
                value: AuctionSnapshot::default(),
            });
        ensure!(
            generation.identity == identity,
            "conflicting auction metadata within one generation"
        );
        if generation.pages[page] {
            return Ok(());
        }
        let merged = (|| {
            ensure!(
                generation.count + meta.count <= identity.2,
                "auction record count exceeded"
            );
            merge_auction_page(state, &mut generation.value, part)?;
            generation.count += meta.count;
            generation.pages[page] = true;
            Ok(())
        })();
        if merged.is_err() {
            // A failed merge may have modified the accumulator; never reuse it.
            self.generations.remove(&identity.0);
        }
        while self
            .generations
            .values()
            .map(|generation| generation.value.items.len())
            .sum::<usize>()
            > state.config.max_price_entries
        {
            self.generations.pop_first();
        }
        merged
    }

    fn missing(&self) -> Vec<usize> {
        self.generations
            .last_key_value()
            .map_or_else(Vec::new, |(_, generation)| {
                generation
                    .pages
                    .iter()
                    .enumerate()
                    .filter_map(|(page, received)| (!received).then_some(page))
                    .collect()
            })
    }

    fn complete(&mut self) -> Option<AuctionGeneration> {
        let updated = self
            .generations
            .iter()
            .rev()
            .find(|(_, generation)| generation.pages.iter().all(|received| *received))
            .map(|(updated, _)| *updated)?;
        self.generations.remove(&updated)
    }

    fn pending(&self) -> Option<AuctionPending> {
        self.generations
            .last_key_value()
            .map(|(updated, generation)| AuctionPending {
                generation: *updated,
                received: generation
                    .pages
                    .iter()
                    .filter(|received| **received)
                    .count(),
                total: generation.pages.len(),
                missing: generation
                    .pages
                    .iter()
                    .enumerate()
                    .filter_map(|(page, received)| (!received).then_some(page))
                    .take(16)
                    .collect(),
                retry_after_secs: 5,
            })
    }
}

pub fn run(state: Arc<State>) -> Result<()> {
    let _directory_lock = persist::lock_directory(&state.config)?;
    persist::restore(&state);
    let mut upstream = Upstream::new(state.clone())?;
    let assembly = Mutex::new(AuctionAssembly::default());
    let mut next = [Instant::now(); 3];
    let intervals = [
        state.config.catalog_interval_secs,
        state.config.bazaar_interval_secs,
        state.config.auction_interval_secs,
    ];
    let mut failures = [0u32; 3];
    while state.running.load(Ordering::Relaxed) {
        for (index, source) in ["catalog", "bazaar", "auctions"].iter().enumerate() {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            if Instant::now() < next[index] {
                continue;
            }
            let started = Instant::now();
            state.refreshing.store(true, Ordering::Relaxed);
            {
                let mut status = state.sync.lock().unwrap_or_else(|v| v.into_inner());
                *status = SyncStatus {
                    source,
                    started_at: now_ms(),
                    ..Default::default()
                };
            }
            let deadline = started + Duration::from_secs(state.config.round_timeout_secs);
            let result = match index {
                0 => catalog(&state, &mut upstream, deadline),
                1 => bazaar(&state, &mut upstream, deadline),
                _ => auctions(&state, &mut upstream, &assembly, deadline),
            };
            state.refreshing.store(false, Ordering::Relaxed);
            let duration = started.elapsed().as_millis() as u64;
            let delay = match result {
                Ok(changed) => {
                    failures[index] = 0;
                    state
                        .counters
                        .sync_successes
                        .fetch_add(1, Ordering::Relaxed);
                    info!(source, duration_ms = duration, changed, "sync completed");
                    intervals[index]
                }
                Err(error) if error.is::<AuctionPending>() => {
                    failures[index] = 0;
                    info!(source, duration_ms = duration, progress = %error,
                        "sync pending; partial auction generations retained");
                    error
                        .downcast_ref::<AuctionPending>()
                        .unwrap()
                        .retry_after_secs
                }
                Err(error) => {
                    failures[index] = (failures[index] + 1).min(6);
                    state.counters.sync_failures.fetch_add(1, Ordering::Relaxed);
                    let message = format!("{error:#}");
                    state
                        .sync
                        .lock()
                        .unwrap_or_else(|v| v.into_inner())
                        .last_error = Some(message.chars().take(512).collect());
                    if state.running.load(Ordering::Relaxed) {
                        warn!(source, error = %message, "sync failed; previous snapshot retained");
                    }
                    (2u64.pow(failures[index]) * 5).min(intervals[index].max(30))
                }
            };
            {
                let mut status = state.sync.lock().unwrap_or_else(|v| v.into_inner());
                status.finished_at = now_ms();
                status.duration_ms = duration;
            }
            // Fixed delay after completion, plus small jitter: no timer backlog or overlapping rounds.
            next[index] = Instant::now()
                + Duration::from_secs(delay)
                + Duration::from_millis(now_ms() % 1000);
        }
        let until = next.iter().copied().min().unwrap_or(Instant::now());
        if wait(&state, until, Instant::now() + Duration::from_secs(86400)).is_err() {
            break;
        }
    }
    Ok(())
}

fn checkpoint<T: serde::Serialize>(state: &State, source: &str, value: &T) {
    if let Err(error) = persist::save(&state.config, source, value) {
        state
            .counters
            .persistence_failures
            .fetch_add(1, Ordering::Relaxed);
        error!(source, error = %error, "checkpoint failed; validated memory snapshot remains available");
    }
}

fn catalog(state: &State, upstream: &mut Upstream, deadline: Instant) -> Result<bool> {
    let mut items = BTreeMap::new();
    let meta = upstream.parse::<RawItem, _>(
        "v2/resources/skyblock/items",
        RecordSet {
            field: "items",
            object: false,
            limit: state.config.max_catalog_items,
        },
        None,
        deadline,
        |_, raw| {
            nbt::validate_id(&raw.id.0)?;
            ensure!(
                raw.npc_sell_price
                    .is_none_or(|v| v.is_finite() && (0.0..=1e16).contains(&v)),
                "invalid NPC sell price"
            );
            let item = Item {
                search: normalized(&format!("{} {}", raw.id.0, raw.name.0)),
                id: raw.id.clone(),
                name: raw.name,
                tier: raw.tier,
                npc_sell_price: raw.npc_sell_price,
                icon: Icon {
                    material: raw.material,
                    durability: raw.durability,
                    color: raw.color,
                    item_model: raw.item_model,
                    skin_texture: raw.skin.and_then(|s| nbt::skin_hash(&s.value.0)),
                    glowing: raw.glowing,
                },
            };
            ensure!(items.insert(raw.id, item).is_none(), "duplicate catalog ID");
            Ok(())
        },
    )?;
    ensure!(!items.is_empty(), "empty catalog");
    let value = CatalogSnapshot {
        last_updated: meta.last_updated.unwrap(),
        fetched_at: now_ms(),
        items,
    };
    checkpoint(state, "catalog", &value);
    state.catalog.store(Arc::new(value));
    Ok(true)
}

fn best_order(orders: &Orders, lowest: bool) -> Result<Option<f64>> {
    let mut best: Option<f64> = None;
    for order in &orders.0 {
        ensure!(
            order.amount.is_finite() && order.amount >= 0.0,
            "invalid bazaar order amount"
        );
        let price = valid_price(order.price)
            .ok_or_else(|| anyhow::anyhow!("invalid bazaar order price"))?;
        if order.amount == 0.0 {
            continue;
        }
        best = Some(match best {
            Some(value) if lowest => value.min(price),
            Some(value) => value.max(price),
            None => price,
        });
    }
    Ok(best)
}

fn volume(value: f64) -> Result<u64> {
    ensure!(
        value.is_finite() && (0.0..=1e16).contains(&value),
        "invalid bazaar volume"
    );
    Ok(value as u64)
}

fn bazaar(state: &State, upstream: &mut Upstream, deadline: Instant) -> Result<bool> {
    let previous = state.bazaar.load().last_updated;
    let mut products = BTreeMap::new();
    let meta = upstream.parse::<RawProduct, _>(
        "v2/skyblock/bazaar",
        RecordSet {
            field: "products",
            object: true,
            limit: state.config.max_bazaar_products,
        },
        (previous > 0).then_some(previous),
        deadline,
        |key, raw| {
            let key = key.unwrap();
            nbt::validate_id(&key.0)?;
            ensure!(key == raw.product_id, "bazaar product ID mismatch");
            // Hypixel buy_summary contains asks (instant-buy); sell_summary contains bids (instant-sell).
            // Use the best order, not quick_status's volume-weighted top-2% average.
            let ask = best_order(&raw.buy_summary, true)?;
            let bid = best_order(&raw.sell_summary, false)?;
            let value = BazaarQuote {
                instant_buy: ask,
                instant_sell: bid,
                buy_order: bid,
                sell_offer: ask,
                buy_volume: volume(raw.quick_status.buy_volume)?,
                sell_volume: volume(raw.quick_status.sell_volume)?,
            };
            ensure!(
                products.insert(key, value).is_none(),
                "duplicate bazaar product"
            );
            Ok(())
        },
    )?;
    if meta.last_updated == Some(previous) {
        return Ok(false);
    }
    ensure!(!products.is_empty(), "empty bazaar");
    let value = BazaarSnapshot {
        last_updated: meta.last_updated.unwrap(),
        fetched_at: now_ms(),
        products,
    };
    checkpoint(state, "bazaar", &value);
    state.bazaar.store(Arc::new(value));
    Ok(true)
}

fn add_auction(
    state: &State,
    snapshot: &mut AuctionSnapshot,
    raw: RawAuction,
    now: u64,
) -> Result<()> {
    if !raw.bin || raw.claimed || raw.end <= now {
        return Ok(());
    }
    snapshot.bin_auctions += 1;
    ensure!(
        raw.starting_bid > 0 && raw.starting_bid <= 10_000_000_000_000_000,
        "invalid BIN price"
    );
    let item = nbt::identity(raw.item_bytes.data(), &state.config)?;
    let Some(id) = item.id else {
        snapshot.skipped_without_id += 1;
        return Ok(());
    };
    let key = item.variant.key(&id);
    let icon = if item.skin_texture.is_some() || id == "PET" {
        Some(Icon {
            material: Text("SKULL_ITEM".to_owned()),
            durability: Some(3),
            skin_texture: item.skin_texture,
            ..Icon::default()
        })
    } else if id == "ENCHANTED_BOOK" {
        Some(Icon {
            material: Text("ENCHANTED_BOOK".to_owned()),
            glowing: true,
            ..Icon::default()
        })
    } else {
        None
    };
    ensure!(key.len() <= 512, "variant key length limit exceeded");
    let price = valid_price(raw.starting_bid as f64 / item.quantity as f64)
        .ok_or_else(|| anyhow::anyhow!("invalid per-unit price"))?;
    if let Some(current) = snapshot.items.get_mut(key.as_str()) {
        current.price.listings = current
            .price
            .listings
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("listing count overflow"))?;
        if price < current.price.lowest_bin
            || (price == current.price.lowest_bin && raw.uuid.0 < current.price.auction_uuid.0)
        {
            current.price.lowest_bin = price;
            current.price.listing_price = raw.starting_bid;
            current.price.quantity = item.quantity;
            current.price.auction_uuid = raw.uuid;
            current.price.ends_at = raw.end;
            current.price.pet_experience = item.pet_experience;
            current.icon = icon;
            current.name = Text(plain_name(&raw.item_name.0));
            current.tier = raw.tier;
            current.search = normalized(&format!("{} {} {}", id, current.name.0, key));
        }
    } else {
        ensure!(
            snapshot.items.len() < state.config.max_price_entries,
            "price entry limit exceeded"
        );
        let name = Text(plain_name(&raw.item_name.0));
        let search = normalized(&format!("{} {} {}", id, name.0, key));
        snapshot.items.insert(
            Text(key),
            AuctionItem {
                id: Text(id),
                name,
                tier: raw.tier,
                variant: item.variant,
                icon,
                price: AuctionQuote {
                    lowest_bin: price,
                    listing_price: raw.starting_bid,
                    quantity: item.quantity,
                    auction_uuid: raw.uuid,
                    ends_at: raw.end,
                    listings: 1,
                    pet_experience: item.pet_experience,
                },
                search,
            },
        );
    }
    Ok(())
}

fn validate_page(state: &State, meta: &Metadata, page: usize) -> Result<(u64, usize, usize)> {
    ensure!(meta.page == Some(page), "auction page number mismatch");
    let identity = (
        meta.last_updated
            .ok_or_else(|| anyhow::anyhow!("missing lastUpdated"))?,
        meta.total_pages
            .ok_or_else(|| anyhow::anyhow!("missing totalPages"))?,
        meta.total_auctions
            .ok_or_else(|| anyhow::anyhow!("missing totalAuctions"))?,
    );
    ensure!(
        identity.1 > 0
            && identity.1 <= state.config.max_auction_pages
            && page < identity.1
            && identity.2 <= state.config.max_auctions,
        "auction pagination limit exceeded"
    );
    Ok(identity)
}

fn auctions(
    state: &State,
    upstream: &mut Upstream,
    assembly: &Mutex<AuctionAssembly>,
    deadline: Instant,
) -> Result<bool> {
    let now = now_ms();
    {
        let mut assembly = assembly.lock().unwrap_or_else(|v| v.into_inner());
        assembly.prune(state);
    }
    if assembly
        .lock()
        .unwrap_or_else(|v| v.into_inner())
        .generations
        .is_empty()
    {
        let (meta, part) = auction_page(state, upstream, 0, now, deadline)?;
        let mut assembly = assembly.lock().unwrap_or_else(|v| v.into_inner());
        assembly.accept(state, 0, meta, part)?;
        if assembly.generations.is_empty() {
            return Ok(false);
        }
        if let Some(mut pending) = assembly
            .pending()
            .filter(|pending| pending.received < pending.total)
        {
            pending.retry_after_secs = 1;
            return Err(pending.into());
        }
    }
    let (pages, expected, more_pages) = {
        let mut assembly = assembly.lock().unwrap_or_else(|v| v.into_inner());
        let mut pages = assembly.missing();
        // One worker wave per scheduler turn. Rotate missing pages so a slow/old
        // low-numbered page cannot starve the tail of the auction list.
        let split = pages.partition_point(|page| *page < assembly.next_page);
        pages.rotate_left(split);
        let more_pages = pages.len() > state.config.auction_page_concurrency;
        pages.truncate(state.config.auction_page_concurrency);
        if let Some(page) = pages.last() {
            assembly.next_page = page + 1;
        }
        (
            pages,
            *assembly.generations.last_key_value().unwrap().0,
            more_pages,
        )
    };
    let next_page = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let failure = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..state.config.auction_page_concurrency.min(pages.len()) {
            let mut worker = upstream.clone();
            let pages = &pages;
            let next_page = &next_page;
            let stopped = &stopped;
            let failure = &failure;
            scope.spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    let index = next_page.fetch_add(1, Ordering::Relaxed);
                    let Some(&page) = pages.get(index) else {
                        break;
                    };
                    let result = auction_page(state, &mut worker, page, now, deadline).and_then(
                        |(meta, part)| {
                            if meta.last_updated != Some(expected) {
                                state
                                    .counters
                                    .snapshot_mismatches
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            // A different timestamp is routed to its own bounded accumulator.
                            // Completed pages survive HTTP failures and generation rollover.
                            assembly
                                .lock()
                                .unwrap_or_else(|v| v.into_inner())
                                .accept(state, page, meta, part)
                        },
                    );
                    if let Err(error) = result {
                        let mut failure = failure.lock().unwrap_or_else(|v| v.into_inner());
                        if failure.is_none() {
                            *failure = Some(error);
                        }
                        stopped.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });
    let completed = assembly
        .lock()
        .unwrap_or_else(|v| v.into_inner())
        .complete();
    let Some(completed) = completed else {
        if let Some(error) = failure.into_inner().unwrap_or_else(|v| v.into_inner()) {
            return Err(error);
        }
        let mut pending = assembly
            .lock()
            .unwrap_or_else(|v| v.into_inner())
            .pending()
            .ok_or_else(|| anyhow::anyhow!("auction assembly missing"))?;
        if more_pages {
            pending.retry_after_secs = 1;
        }
        return Err(pending.into());
    };
    ensure!(
        completed.count == completed.identity.2,
        "auction record count mismatch"
    );
    let mut value = completed.value;
    let published_at = now_ms();
    value
        .items
        .retain(|_, item| item.price.ends_at > published_at);
    value.last_updated = completed.identity.0;
    value.fetched_at = published_at;
    value.total_auctions = completed.count;
    checkpoint(state, "auctions", &value);
    state.auctions.store(Arc::new(value));
    assembly
        .lock()
        .unwrap_or_else(|v| v.into_inner())
        .prune(state);
    Ok(true)
}

fn auction_page(
    state: &State,
    upstream: &mut Upstream,
    page: usize,
    now: u64,
    deadline: Instant,
) -> Result<(Metadata, AuctionSnapshot)> {
    let mut part = AuctionSnapshot::default();
    let meta = upstream.parse::<RawAuction, _>(
        &format!("v2/skyblock/auctions?page={page}"),
        RecordSet {
            field: "auctions",
            object: false,
            limit: state.config.max_auctions.min(2000),
        },
        None,
        deadline,
        |_, raw| add_auction(state, &mut part, raw, now),
    )?;
    validate_page(state, &meta, page)?;
    Ok((meta, part))
}

fn merge_auction_page(
    state: &State,
    target: &mut AuctionSnapshot,
    part: AuctionSnapshot,
) -> Result<()> {
    target.bin_auctions += part.bin_auctions;
    target.skipped_without_id += part.skipped_without_id;
    for (key, mut item) in part.items {
        if let Some(current) = target.items.get_mut(&key) {
            let listings = current
                .price
                .listings
                .checked_add(item.price.listings)
                .ok_or_else(|| anyhow::anyhow!("listing count overflow"))?;
            if item.price.lowest_bin < current.price.lowest_bin
                || (item.price.lowest_bin == current.price.lowest_bin
                    && item.price.auction_uuid < current.price.auction_uuid)
            {
                item.price.listings = listings;
                *current = item;
            } else {
                current.price.listings = listings;
            }
        } else {
            ensure!(
                target.items.len() < state.config.max_price_entries,
                "price entry limit exceeded"
            );
            target.items.insert(key, item);
        }
    }
    Ok(())
}
