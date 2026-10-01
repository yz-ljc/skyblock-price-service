use std::{fmt, io::Read};

use anyhow::{Result, ensure};
use serde::{
    Deserializer,
    de::{self, DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use crate::model::Text;

#[derive(Default, Debug)]
pub struct Metadata {
    pub success: Option<bool>,
    pub last_updated: Option<u64>,
    pub page: Option<usize>,
    pub total_pages: Option<usize>,
    pub total_auctions: Option<usize>,
    pub count: usize,
}

/// The deserializer visits one record at a time, and skips unrelated JSON fields.
/// The caller supplies a byte/deadline-limited reader and publishes only after validation.
pub fn parse<R, T, F>(
    reader: R,
    field: &'static str,
    object: bool,
    limit: usize,
    skip_updated: Option<u64>,
    mut consume: F,
) -> Result<Metadata>
where
    R: Read,
    T: DeserializeOwned,
    F: FnMut(Option<Text<128>>, T) -> Result<()>,
{
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let metadata = Root::<T, F> {
        field,
        object,
        limit,
        skip_updated,
        consume: &mut consume,
        marker: std::marker::PhantomData,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    ensure!(
        metadata.success == Some(true),
        "upstream success was not true"
    );
    ensure!(
        metadata.last_updated.is_some_and(|v| v > 0),
        "upstream timestamp missing"
    );
    Ok(metadata)
}

struct Root<'a, T, F> {
    field: &'static str,
    object: bool,
    limit: usize,
    skip_updated: Option<u64>,
    consume: &'a mut F,
    marker: std::marker::PhantomData<T>,
}

impl<'de, T, F> DeserializeSeed<'de> for Root<'_, T, F>
where
    T: DeserializeOwned,
    F: FnMut(Option<Text<128>>, T) -> Result<()>,
{
    type Value = Metadata;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Metadata, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, T, F> Visitor<'de> for Root<'_, T, F>
where
    T: DeserializeOwned,
    F: FnMut(Option<Text<128>>, T) -> Result<()>,
{
    type Value = Metadata;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an official API response")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Metadata, M::Error> {
        let mut metadata = Metadata::default();
        let mut records_seen = false;
        while let Some(key) = map.next_key::<Text<64>>()? {
            match key.0.as_str() {
                "success" if metadata.success.is_none() => {
                    metadata.success = Some(map.next_value()?)
                }
                "lastUpdated" if metadata.last_updated.is_none() => {
                    metadata.last_updated = Some(map.next_value()?)
                }
                "page" if metadata.page.is_none() => metadata.page = Some(map.next_value()?),
                "totalPages" if metadata.total_pages.is_none() => {
                    metadata.total_pages = Some(map.next_value()?)
                }
                "totalAuctions" if metadata.total_auctions.is_none() => {
                    metadata.total_auctions = Some(map.next_value()?)
                }
                key if key == self.field && !records_seen => {
                    records_seen = true;
                    if self.skip_updated.is_some() && self.skip_updated == metadata.last_updated {
                        map.next_value::<IgnoredAny>()?;
                    } else {
                        metadata.count = map.next_value_seed(Records::<T, F> {
                            object: self.object,
                            limit: self.limit,
                            consume: self.consume,
                            marker: std::marker::PhantomData,
                        })?;
                    }
                }
                "success" | "lastUpdated" | "page" | "totalPages" | "totalAuctions" => {
                    return Err(de::Error::custom("duplicate metadata field"));
                }
                key if key == self.field => {
                    return Err(de::Error::custom("duplicate records field"));
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if !records_seen {
            return Err(de::Error::custom("records field missing"));
        }
        Ok(metadata)
    }
}

struct Records<'a, T, F> {
    object: bool,
    limit: usize,
    consume: &'a mut F,
    marker: std::marker::PhantomData<T>,
}

impl<'de, T, F> DeserializeSeed<'de> for Records<'_, T, F>
where
    T: DeserializeOwned,
    F: FnMut(Option<Text<128>>, T) -> Result<()>,
{
    type Value = usize;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        if self.object {
            deserializer.deserialize_map(self)
        } else {
            deserializer.deserialize_seq(self)
        }
    }
}

impl<'de, T, F> Visitor<'de> for Records<'_, T, F>
where
    T: DeserializeOwned,
    F: FnMut(Option<Text<128>>, T) -> Result<()>,
{
    type Value = usize;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded records")
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<usize, S::Error> {
        let mut count = 0;
        while let Some(record) = seq.next_element::<T>()? {
            count += 1;
            if count > self.limit {
                return Err(de::Error::custom("record count limit exceeded"));
            }
            (self.consume)(None, record).map_err(de::Error::custom)?;
        }
        Ok(count)
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<usize, M::Error> {
        let mut count = 0;
        while let Some(key) = map.next_key::<Text<128>>()? {
            count += 1;
            if count > self.limit {
                return Err(de::Error::custom("record count limit exceeded"));
            }
            (self.consume)(Some(key), map.next_value::<T>()?).map_err(de::Error::custom)?;
        }
        Ok(count)
    }
}
