use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State as AppState, rejection::QueryRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    model::{
        AuctionQuote, AuctionSnapshot, BazaarQuote, BazaarSnapshot, CatalogSnapshot, Icon, Variant,
        normalized, now_ms,
    },
    pagination::{self, Session},
    state::State,
};

pub fn router(state: Arc<State>) -> Router {
    let protected = Router::new()
        .route("/v1/items/search", get(search))
        .route("/v1/items/{id}/price", get(price))
        .route("/v1/status", get(status))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), gate));
    Router::new()
        .merge(protected)
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .fallback(|| async { failure(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint") })
        .with_state(state)
}

fn success<T: Serialize>(data: T) -> Response {
    Json(json!({ "data": data, "message": "ok", "status": 200, "timestamp": now_ms() }))
        .into_response()
}

fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "data": null, "message": message, "code": code, "status": status.as_u16(), "timestamp": now_ms() }))).into_response()
}

async fn gate(AppState(state): AppState<Arc<State>>, request: Request, next: Next) -> Response {
    if request.uri().to_string().len() > 2048 {
        return failure(
            StatusCode::URI_TOO_LONG,
            "request_too_large",
            "Request URI exceeds 2048 bytes",
        );
    }
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let mut difference = supplied.len() ^ state.token.len();
    for (a, b) in supplied.bytes().zip(state.token.bytes()) {
        difference |= (a ^ b) as usize;
    }
    if difference != 0 {
        return failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Invalid access token",
        );
    }
    let Ok(_permit) = state.query_slots.clone().try_acquire_owned() else {
        state
            .counters
            .query_rejections
            .fetch_add(1, Ordering::Relaxed);
        let mut response = failure(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "Too many concurrent queries",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, "1".parse().unwrap());
        return response;
    };
    state.counters.queries.fetch_add(1, Ordering::Relaxed);
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[derive(Clone, Serialize)]
pub struct SourceMeta {
    available: bool,
    last_updated: u64,
    fetched_at: u64,
    age_seconds: Option<u64>,
    stale: bool,
}

fn source_meta(
    state: &State,
    updated: u64,
    fetched: u64,
    now: u64,
    stale_after_secs: u64,
) -> SourceMeta {
    let age = (updated > 0).then(|| now.saturating_sub(updated) / 1000);
    SourceMeta {
        available: age.is_some_and(|v| v <= state.config.max_price_age_secs),
        last_updated: updated,
        fetched_at: fetched,
        age_seconds: age,
        stale: age.is_none_or(|v| v > stale_after_secs),
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Sources {
    source: &'static str,
    catalog_last_updated: u64,
    bazaar: SourceMeta,
    auctions: SourceMeta,
    skipped_auctions_without_id: usize,
    skipped_auctions_invalid: usize,
}

fn sources(
    state: &State,
    catalog: &CatalogSnapshot,
    bazaar: &BazaarSnapshot,
    auctions: &AuctionSnapshot,
    now: u64,
) -> Sources {
    Sources {
        source: "Hypixel official API",
        catalog_last_updated: catalog.last_updated,
        bazaar: source_meta(
            state,
            bazaar.last_updated,
            bazaar.fetched_at,
            now,
            state.config.stale_after_secs,
        ),
        auctions: source_meta(
            state,
            auctions.last_updated,
            auctions.fetched_at,
            now,
            state.config.auction_stale_after_secs,
        ),
        skipped_auctions_without_id: auctions.skipped_without_id,
        skipped_auctions_invalid: auctions.skipped_invalid,
    }
}

#[derive(Deserialize, Default)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
    cursor: Option<String>,
    market: Option<Market>,
    #[serde(default)]
    only_priced: bool,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Market {
    Bazaar,
    Auction,
    Npc,
}
#[derive(Deserialize, Default)]
struct PriceParams {
    variant: Option<String>,
}

#[derive(Serialize)]
struct ItemResult {
    key: String,
    id: String,
    name: String,
    tier: Option<String>,
    variant: Option<Variant>,
    icon_key: String,
    icon: Option<Icon>,
    npc_sell_price: Option<f64>,
    bazaar: Option<BazaarQuote>,
    auction: Option<AuctionQuote>,
    price_basis: &'static str,
}

fn result(
    key: &str,
    catalog: &CatalogSnapshot,
    bazaar: &BazaarSnapshot,
    auctions: &AuctionSnapshot,
    sources: &Sources,
    now: u64,
) -> ItemResult {
    let id = key.split('|').next().unwrap_or(key);
    let item = catalog.items.get(id);
    let auction = auctions.items.get(key);
    ItemResult {
        key: key.to_owned(),
        id: id.to_owned(),
        name: item
            .map(|v| v.name.0.as_str())
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                auction
                    .map(|v| v.name.0.as_str())
                    .filter(|name| !name.trim().is_empty())
            })
            .unwrap_or_else(|| {
                if crate::nbt::validate_id(id).is_ok() {
                    id
                } else {
                    "Unknown Item"
                }
            })
            .to_owned(),
        tier: auction
            .and_then(|v| v.tier.as_ref())
            .or_else(|| item.and_then(|v| v.tier.as_ref()))
            .map(|v| v.0.clone()),
        variant: auction
            .filter(|_| key.contains('|'))
            .map(|v| v.variant.clone()),
        icon_key: key.to_owned(),
        icon: item
            .map(|v| v.icon.clone())
            .or_else(|| auction.and_then(|v| v.icon.clone())),
        npc_sell_price: item.and_then(|v| v.npc_sell_price),
        bazaar: sources
            .bazaar
            .available
            .then(|| bazaar.products.get(id).cloned())
            .flatten(),
        auction: sources
            .auctions
            .available
            .then(|| {
                auction
                    .filter(|v| v.price.ends_at > now)
                    .map(|v| v.price.clone())
            })
            .flatten(),
        price_basis: if auction.is_some_and(|v| v.variant.pet_type.is_some()) {
            "lowest_bin_per_unit_across_pet_levels"
        } else {
            "lowest_bin_per_unit_no_upgrade_valuation"
        },
    }
}

fn has_price(
    key: &str,
    catalog: &CatalogSnapshot,
    bazaar: &BazaarSnapshot,
    auctions: &AuctionSnapshot,
    sources: &Sources,
    now: u64,
    market: Option<Market>,
) -> bool {
    let id = key.split('|').next().unwrap_or(key);
    let bz = sources.bazaar.available
        && bazaar
            .products
            .get(id)
            .is_some_and(|v| v.instant_buy.is_some() || v.instant_sell.is_some());
    let ah = sources.auctions.available
        && auctions
            .items
            .get(key)
            .is_some_and(|v| v.price.ends_at > now);
    let npc = catalog
        .items
        .get(id)
        .is_some_and(|v| v.npc_sell_price.is_some());
    match market {
        Some(Market::Bazaar) => bz,
        Some(Market::Auction) => ah,
        Some(Market::Npc) => npc,
        None => bz || ah || npc,
    }
}

// Prices and ordering belong to the initial search. Age and expiry still advance
// on every page so a retained search cannot present an expired BIN as available.
fn search_page(
    state: &State,
    session: &Session<Sources>,
    id: &str,
    offset: usize,
    limit: usize,
    query: &str,
) -> Response {
    let total = session.rows.len();
    if offset % limit != 0 || (offset > 0 && offset >= total) {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "Invalid cursor offset",
        );
    }
    let now = now_ms();
    let mut sources = session.metadata.clone();
    sources.bazaar = source_meta(
        state,
        sources.bazaar.last_updated,
        sources.bazaar.fetched_at,
        now,
        state.config.stale_after_secs,
    );
    sources.auctions = source_meta(
        state,
        sources.auctions.last_updated,
        sources.auctions.fetched_at,
        now,
        state.config.auction_stale_after_secs,
    );
    let mut rows = Vec::with_capacity(limit);
    for row in session.rows.iter().skip(offset).take(limit) {
        let mut row: serde_json::Value = match serde_json::from_str(row) {
            Ok(row) => row,
            Err(_) => {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid_search_cache",
                    "Cannot read cached search",
                );
            }
        };
        if !sources.bazaar.available {
            row["bazaar"] = serde_json::Value::Null;
        }
        if !sources.auctions.available
            || row["auction"]["ends_at"]
                .as_u64()
                .is_some_and(|end| end <= now)
        {
            row["auction"] = serde_json::Value::Null;
        }
        rows.push(row);
    }
    let next_cursor =
        (offset + rows.len() < total).then(|| format!("{id}:{}", offset + rows.len()));
    success(json!({ "query": query, "total": total, "items": rows,
        "next_cursor": next_cursor, "sources": sources }))
}

async fn search(
    AppState(state): AppState<Arc<State>>,
    query: Result<Query<SearchParams>, QueryRejection>,
) -> Response {
    let params = match query {
        Ok(Query(p)) => p,
        Err(_) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                "Invalid search parameters",
            );
        }
    };
    let q = normalized(&params.q);
    if params.q.len() > 128 || q.is_empty() {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "q must contain 1 to 128 bytes of searchable text",
        );
    }
    let limit = params.limit.unwrap_or(20);
    if !(1..=50).contains(&limit) {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_limit",
            "limit must be 1 to 50",
        );
    }
    // Bind cursors to the normalized query, filters AND page size, not refresh timestamps.
    let context = format!("{q}\n{:?}\n{}\n{limit}", params.market, params.only_priced);
    let mut searches = state.searches.lock().unwrap_or_else(|v| v.into_inner());
    searches.prune();
    if let Some(cursor) = &params.cursor {
        let parsed = cursor
            .split_once(':')
            .filter(|(id, index)| {
                id.len() == 16
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                    && !index.is_empty()
                    && index.bytes().all(|b| b.is_ascii_digit())
            })
            .and_then(|(id, index)| index.parse::<usize>().ok().map(|offset| (id, offset)));
        let Some((id, offset)) = parsed.filter(|_| cursor.len() <= 64) else {
            return failure(StatusCode::BAD_REQUEST, "invalid_cursor", "Invalid cursor");
        };
        let Some(session) = searches.get(id) else {
            return failure(
                StatusCode::CONFLICT,
                "search_expired",
                "Search expired or was evicted; restart search without cursor",
            );
        };
        if session.context != context {
            return failure(
                StatusCode::BAD_REQUEST,
                "cursor_query_mismatch",
                "Keep the same query, filters and limit when paging",
            );
        }
        return search_page(&state, session, id, offset, limit, &params.q);
    }
    let catalog = state.catalog.load_full();
    let bazaar = state.bazaar.load_full();
    let auctions = state.auctions.load_full();
    if catalog.items.is_empty() {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "warming_up",
            "Item catalog is not available yet",
        );
    }
    let now = now_ms();
    let sources = sources(&state, &catalog, &bazaar, &auctions, now);
    let words: Vec<_> = q.split_whitespace().collect();
    let mut c = catalog.items.keys().map(|v| v.0.as_str()).peekable();
    let mut b = bazaar.products.keys().map(|v| v.0.as_str()).peekable();
    let mut a = auctions.items.keys().map(|v| v.0.as_str()).peekable();
    let mut rows = Vec::new();
    let mut bytes = 1024; // Includes per-session metadata and bounded query text.
    // Merge indexes once per search; subsequent pages read the compact frozen rows.
    loop {
        let key = [c.peek().copied(), b.peek().copied(), a.peek().copied()]
            .into_iter()
            .flatten()
            .min();
        let Some(key) = key else {
            break;
        };
        if c.peek().copied() == Some(key) {
            c.next();
        }
        if b.peek().copied() == Some(key) {
            b.next();
        }
        if a.peek().copied() == Some(key) {
            a.next();
        }
        let fallback;
        let haystack = if let Some(item) = catalog.items.get(key) {
            item.search.as_str()
        } else if let Some(item) = auctions.items.get(key) {
            item.search.as_str()
        } else {
            fallback = normalized(key);
            &fallback
        };
        if !words.iter().all(|word| haystack.contains(word)) {
            continue;
        }
        if (params.only_priced || params.market.is_some())
            && !has_price(
                key,
                &catalog,
                &bazaar,
                &auctions,
                &sources,
                now,
                params.market,
            )
        {
            continue;
        }
        let row = match serde_json::to_string(&result(
            key, &catalog, &bazaar, &auctions, &sources, now,
        )) {
            Ok(row) => row.into_boxed_str(),
            Err(_) => {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "search_encoding_failed",
                    "Cannot encode search",
                );
            }
        };
        bytes += row.len() + std::mem::size_of::<Box<str>>();
        if bytes > pagination::MAX_SESSION_BYTES {
            return failure(
                StatusCode::BAD_REQUEST,
                "search_too_broad",
                "Too many results; use a more specific query",
            );
        }
        rows.push(row);
    }
    let session = Session {
        context,
        rows: rows.into_boxed_slice(),
        metadata: sources,
        created: Instant::now(),
        bytes,
    };
    // Single-page searches do not need a retained session.
    if session.rows.len() <= limit {
        return search_page(&state, &session, "", 0, limit, &params.q);
    }
    let id = searches.insert(session);
    search_page(
        &state,
        searches.get(&id).expect("just inserted search"),
        &id,
        0,
        limit,
        &params.q,
    )
}

async fn price(
    AppState(state): AppState<Arc<State>>,
    Path(id): Path<String>,
    query: Result<Query<PriceParams>, QueryRejection>,
) -> Response {
    if crate::nbt::validate_id(&id).is_err() {
        return failure(StatusCode::BAD_REQUEST, "invalid_id", "Invalid item ID");
    }
    let params = match query {
        Ok(Query(p)) => p,
        Err(_) => {
            return failure(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                "Invalid price parameters",
            );
        }
    };
    let key = params.variant.as_deref().unwrap_or(&id);
    if key.len() > 512 || key.split('|').next() != Some(id.as_str()) {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_variant",
            "variant must be a complete search result key for this ID",
        );
    }
    let catalog = state.catalog.load_full();
    let bazaar = state.bazaar.load_full();
    let auctions = state.auctions.load_full();
    if catalog.items.is_empty() {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "warming_up",
            "Item catalog is not available yet",
        );
    }
    if key == id {
        if !catalog.items.contains_key(id.as_str())
            && !bazaar.products.contains_key(id.as_str())
            && !auctions.items.contains_key(key)
        {
            return failure(
                StatusCode::NOT_FOUND,
                "item_not_found",
                "Item not found; use search to find variant keys",
            );
        }
    } else if !auctions.items.contains_key(key) {
        return failure(
            StatusCode::NOT_FOUND,
            "variant_not_found",
            "Variant not found",
        );
    }
    let now = now_ms();
    let sources = sources(&state, &catalog, &bazaar, &auctions, now);
    success(
        json!({ "item": result(key, &catalog, &bazaar, &auctions, &sources, now), "sources": sources }),
    )
}

async fn live(AppState(state): AppState<Arc<State>>) -> Response {
    if state.running.load(Ordering::Relaxed) {
        success(json!({ "alive": true }))
    } else {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "stopping",
            "Service is stopping",
        )
    }
}

async fn ready(AppState(state): AppState<Arc<State>>) -> Response {
    let catalog = state.catalog.load();
    let bazaar = state.bazaar.load();
    let auctions = state.auctions.load();
    let now = now_ms();
    let available = state.running.load(Ordering::Relaxed)
        && !catalog.items.is_empty()
        && now.saturating_sub(catalog.fetched_at) / 1000 <= state.config.max_restore_age_secs
        && !source_meta(
            &state,
            bazaar.last_updated,
            bazaar.fetched_at,
            now,
            state.config.stale_after_secs,
        )
        .stale
        && !source_meta(
            &state,
            auctions.last_updated,
            auctions.fetched_at,
            now,
            state.config.auction_stale_after_secs,
        )
        .stale;
    if available {
        success(json!({ "ready": true }))
    } else {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "Fresh catalog, bazaar and auction snapshots are required",
        )
    }
}

async fn status(AppState(state): AppState<Arc<State>>) -> Response {
    let catalog = state.catalog.load();
    let bazaar = state.bazaar.load();
    let auctions = state.auctions.load();
    let sync = state.sync.lock().unwrap_or_else(|v| v.into_inner()).clone();
    success(json!({ "uptime_seconds": state.started.elapsed().as_secs(),
        "refreshing": state.refreshing.load(Ordering::Relaxed),
        "catalog_items": catalog.items.len(), "bazaar_products": bazaar.products.len(), "auction_price_entries": auctions.items.len(),
        "total_auctions": auctions.total_auctions, "bin_auctions": auctions.bin_auctions,
        "sources": sources(&state, &catalog, &bazaar, &auctions, now_ms()),
        "last_sync": { "source": sync.source, "started_at": sync.started_at, "finished_at": sync.finished_at,
            "duration_ms": sync.duration_ms, "error": sync.last_error },
        "limits": { "max_price_entries": state.config.max_price_entries, "max_response_bytes": state.config.max_response_bytes,
            "request_timeout_secs": state.config.request_timeout_secs, "response_timeout_secs": state.config.response_timeout_secs,
            "request_spacing_ms": state.config.request_spacing_ms, "request_retries": state.config.request_retries,
            "auction_page_concurrency": state.config.auction_page_concurrency, "round_timeout_secs": state.config.round_timeout_secs,
            "max_nbt_decoded_bytes": state.config.max_nbt_decoded_bytes, "max_query_concurrency": state.config.max_query_concurrency }
    }))
}

async fn metrics(AppState(state): AppState<Arc<State>>) -> Response {
    let mut lines = String::new();
    for (name, counter) in [
        ("upstream_requests_total", &state.counters.upstream_requests),
        ("upstream_429_total", &state.counters.upstream_429),
        ("sync_successes_total", &state.counters.sync_successes),
        ("sync_failures_total", &state.counters.sync_failures),
        (
            "snapshot_mismatches_total",
            &state.counters.snapshot_mismatches,
        ),
        (
            "persistence_failures_total",
            &state.counters.persistence_failures,
        ),
        ("queries_total", &state.counters.queries),
        ("query_rejections_total", &state.counters.query_rejections),
    ] {
        lines.push_str(&format!(
            "# TYPE skyblock_{name} counter\nskyblock_{name} {}\n",
            counter.load(Ordering::Relaxed)
        ));
    }
    for (name, value) in [
        ("catalog_items", state.catalog.load().items.len() as u64),
        ("bazaar_products", state.bazaar.load().products.len() as u64),
        (
            "auction_price_entries",
            state.auctions.load().items.len() as u64,
        ),
        (
            "refreshing",
            state.refreshing.load(Ordering::Relaxed) as u64,
        ),
    ] {
        lines.push_str(&format!(
            "# TYPE skyblock_{name} gauge\nskyblock_{name} {value}\n"
        ));
    }
    let now = now_ms();
    for (source, updated) in [
        ("bazaar", state.bazaar.load().last_updated),
        ("auctions", state.auctions.load().last_updated),
    ] {
        lines.push_str(&format!(
            "skyblock_snapshot_initialized{{source=\"{source}\"}} {}\n",
            (updated > 0) as u8
        ));
        if updated > 0 {
            lines.push_str(&format!(
                "skyblock_snapshot_age_seconds{{source=\"{source}\"}} {}\n",
                now.saturating_sub(updated) / 1000
            ));
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(kb) = status.lines().find_map(|v| {
            v.strip_prefix("VmRSS:")
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        }) {
            lines.push_str(&format!("process_resident_memory_bytes {}\n", kb * 1024));
        }
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        lines,
    )
        .into_response()
}
