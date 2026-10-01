use std::{
    collections::BTreeMap,
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, Visitor},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Bounds individual strings before they enter retained data structures.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Text<const N: usize>(pub String);

impl<const N: usize> std::borrow::Borrow<str> for Text<N> {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl<'de, const N: usize> Deserialize<'de> for Text<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Bounded<const N: usize>;
        impl<const N: usize> Visitor<'_> for Bounded<N> {
            type Value = Text<N>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a string of at most {N} bytes")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > N {
                    return Err(E::custom("string byte budget exceeded"));
                }
                Ok(Text(value.to_owned()))
            }
        }
        deserializer.deserialize_str(Bounded::<N>)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Icon {
    pub material: Text<96>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durability: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Text<32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_model: Option<Text<256>>,
    /// Mojang texture hash only; raw base64/signatures are discarded during import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skin_texture: Option<Text<64>>,
    pub glowing: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Item {
    pub id: Text<128>,
    pub name: Text<256>,
    pub tier: Option<Text<32>>,
    pub npc_sell_price: Option<f64>,
    pub icon: Icon,
    #[serde(skip)]
    pub search: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BazaarQuote {
    pub instant_buy: Option<f64>,
    pub instant_sell: Option<f64>,
    pub buy_order: Option<f64>,
    pub sell_offer: Option<f64>,
    pub buy_volume: u64,
    pub sell_volume: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Variant {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pet_type: Option<Text<64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pet_tier: Option<Text<32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pet_skin: Option<Text<128>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub enchantments: BTreeMap<Text<64>, u32>,
}

impl Variant {
    pub fn key(&self, id: &str) -> String {
        let mut key = id.to_owned();
        if let Some(pet) = &self.pet_type {
            key.push_str(&format!(
                "|pet={}|tier={}|skin={}",
                pet.0,
                self.pet_tier.as_ref().map_or("", |v| v.0.as_str()),
                self.pet_skin.as_ref().map_or("", |v| v.0.as_str())
            ));
        }
        for (name, level) in &self.enchantments {
            key.push_str(&format!("|{}={level}", name.0));
        }
        key
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuctionQuote {
    pub lowest_bin: f64,
    pub listing_price: u64,
    pub quantity: u32,
    pub auction_uuid: Text<64>,
    pub ends_at: u64,
    pub listings: u32,
    /// Pets are grouped by type/tier/skin, not level. This is the winning listing's XP.
    pub pet_experience: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuctionItem {
    pub id: Text<128>,
    pub name: Text<256>,
    pub tier: Option<Text<32>>,
    pub variant: Variant,
    #[serde(default)]
    pub icon: Option<Icon>,
    pub price: AuctionQuote,
    #[serde(skip)]
    pub search: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CatalogSnapshot {
    pub last_updated: u64,
    pub fetched_at: u64,
    pub items: BTreeMap<Text<128>, Item>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BazaarSnapshot {
    pub last_updated: u64,
    pub fetched_at: u64,
    pub products: BTreeMap<Text<128>, BazaarQuote>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AuctionSnapshot {
    pub last_updated: u64,
    pub fetched_at: u64,
    pub total_auctions: usize,
    pub bin_auctions: usize,
    pub skipped_without_id: usize,
    pub items: BTreeMap<Text<512>, AuctionItem>,
}

pub fn normalized(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
            continue;
        }
        if c.is_whitespace() || matches!(c, '_' | '-' | ':' | '|' | '=') {
            if !result.is_empty() && !result.ends_with(' ') {
                result.push(' ');
            }
        } else {
            result.extend(c.to_lowercase());
        }
    }
    result.trim().to_owned()
}

pub fn plain_name(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            result.push(c);
        }
    }
    result
}

pub fn valid_price(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0 && value <= 1e16).then_some(value)
}
