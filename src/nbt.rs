use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use serde::Deserialize;

use crate::{
    config::Config,
    model::{Text, Variant},
};

#[derive(Default)]
pub struct ItemIdentity {
    pub id: Option<String>,
    pub quantity: u32,
    pub variant: Variant,
    pub pet_experience: Option<f64>,
    pub skin_texture: Option<Text<64>>,
}

#[derive(Deserialize)]
struct PetInfo {
    #[serde(rename = "type")]
    pet_type: Text<64>,
    tier: Text<32>,
    skin: Option<Text<128>>,
    exp: f64,
}

/// No general NBT tree is allocated. Array lengths, recursion, strings and gzip output are bounded.
pub fn identity(encoded: &str, config: &Config) -> Result<ItemIdentity> {
    ensure!(
        encoded.len() <= config.max_nbt_encoded_bytes,
        "NBT encoded size limit exceeded"
    );
    let compressed = STANDARD.decode(encoded).context("invalid NBT base64")?;
    let mut bytes = Vec::new();
    GzDecoder::new(compressed.as_slice())
        .take(config.max_nbt_decoded_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= config.max_nbt_decoded_bytes,
        "NBT decompressed size limit exceeded"
    );
    let mut parser = Parser {
        bytes: &bytes,
        cursor: 0,
        nodes: 0,
        output: ItemIdentity::default(),
        pet_info: None,
    };
    ensure!(parser.byte()? == 10, "NBT root must be compound");
    parser.string()?;
    parser.value(10, &mut Vec::new(), 0)?;
    ensure!(parser.cursor == bytes.len(), "NBT trailing bytes");
    ensure!(
        (1..=127).contains(&parser.output.quantity),
        "invalid item stack count"
    );
    if parser.output.id.as_deref() == Some("PET") {
        let pet: PetInfo =
            serde_json::from_str(parser.pet_info.as_deref().context("PET missing petInfo")?)?;
        ensure!(
            pet.exp.is_finite() && (0.0..=1e16).contains(&pet.exp),
            "invalid pet experience"
        );
        validate_component(&pet.pet_type.0)?;
        validate_component(&pet.tier.0)?;
        if let Some(skin) = &pet.skin {
            validate_component(&skin.0)?;
        }
        parser.output.variant.pet_type = Some(pet.pet_type);
        parser.output.variant.pet_tier = Some(pet.tier);
        parser.output.variant.pet_skin = pet.skin;
        parser.output.pet_experience = Some(pet.exp);
    }
    if parser.output.id.as_deref() != Some("ENCHANTED_BOOK") {
        parser.output.variant.enchantments.clear();
    }
    if let Some(id) = &parser.output.id {
        validate_id(id)?;
    }
    Ok(parser.output)
}

pub fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.')),
        "invalid item ID"
    );
    Ok(())
}

fn validate_component(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid variant component"
    );
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    nodes: usize,
    output: ItemIdentity,
    pet_info: Option<String>,
}

impl Parser<'_> {
    fn take(&mut self, length: usize) -> Result<&[u8]> {
        let end = self
            .cursor
            .checked_add(length)
            .context("NBT length overflow")?;
        ensure!(end <= self.bytes.len(), "truncated NBT");
        let result = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(result)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn short(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn int(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn string(&mut self) -> Result<String> {
        let length = self.short()? as usize;
        ensure!(length <= 16384, "NBT string limit exceeded");
        // NBT uses modified UTF-8. Relevant ID/pet/enchantment fields are ASCII;
        // unrelated names/lore can contain surrogate encodings and are skipped below.
        Ok(std::str::from_utf8(self.take(length)?)
            .context("invalid NBT identifier UTF-8")?
            .to_owned())
    }

    fn skip_string(&mut self) -> Result<()> {
        let length = self.short()? as usize;
        self.take(length)?;
        Ok(())
    }

    fn length(&mut self) -> Result<usize> {
        let value = self.int()?;
        ensure!(
            (0..=65536).contains(&value),
            "invalid NBT collection length"
        );
        Ok(value as usize)
    }

    fn value(&mut self, tag: u8, path: &mut Vec<String>, depth: usize) -> Result<()> {
        self.nodes += 1;
        ensure!(
            depth <= 32 && self.nodes <= 65536,
            "NBT depth/node limit exceeded"
        );
        let field = path.last().map_or("", String::as_str);
        let extra_attributes =
            path.len() == 4 && path[0] == "i" && path[1] == "tag" && path[2] == "ExtraAttributes";
        match tag {
            1 => {
                let value = self.byte()?;
                if field == "Count" && path.len() == 2 && path[0] == "i" {
                    self.output.quantity = value as u32;
                }
            }
            2 => {
                self.take(2)?;
            }
            3 => {
                let value = self.int()?;
                if path.len() == 5
                    && path[0] == "i"
                    && path[1] == "tag"
                    && path[2] == "ExtraAttributes"
                    && path[3] == "enchantments"
                {
                    ensure!(value > 0 && value <= 1000, "invalid enchantment level");
                    validate_component(field)?;
                    ensure!(
                        self.output.variant.enchantments.len() < 32,
                        "too many enchantments"
                    );
                    self.output
                        .variant
                        .enchantments
                        .insert(Text(field.to_owned()), value as u32);
                }
            }
            4 | 6 => {
                self.take(8)?;
            }
            5 => {
                self.take(4)?;
            }
            7 | 11 | 12 => {
                let length = self.length()?;
                let width = match tag {
                    11 => 4,
                    12 => 8,
                    _ => 1,
                };
                self.take(
                    length
                        .checked_mul(width)
                        .context("NBT array size overflow")?,
                )?;
            }
            8 => {
                if extra_attributes && field == "id" {
                    let value = self.string()?;
                    ensure!(
                        value.len() <= 128 && self.output.id.is_none(),
                        "duplicate/oversized item ID"
                    );
                    self.output.id = Some(value);
                } else if extra_attributes && field == "petInfo" {
                    ensure!(self.pet_info.is_none(), "duplicate petInfo");
                    self.pet_info = Some(self.string()?);
                } else if path.iter().map(String::as_str).eq([
                    "i",
                    "tag",
                    "SkullOwner",
                    "Properties",
                    "textures",
                    "Value",
                ]) {
                    let value = self.string()?;
                    self.output.skin_texture = skin_hash(&value);
                } else {
                    self.skip_string()?;
                }
            }
            9 => {
                let child = self.byte()?;
                let length = self.length()?;
                ensure!(
                    child <= 12 && (child != 0 || length == 0),
                    "invalid NBT list tag"
                );
                if field == "i" && path.len() == 1 {
                    ensure!(length == 1, "expected a single auction item");
                }
                for _ in 0..length {
                    self.value(child, path, depth + 1)?;
                }
            }
            10 => loop {
                let child = self.byte()?;
                if child == 0 {
                    break;
                }
                ensure!(child <= 12, "invalid NBT compound tag");
                let name = self.string()?;
                ensure!(name.len() <= 128, "NBT key limit exceeded");
                path.push(name);
                self.value(child, path, depth + 1)?;
                path.pop();
            },
            _ => bail!("unsupported NBT tag"),
        }
        Ok(())
    }
}

pub fn skin_hash(value: &str) -> Option<Text<64>> {
    let bytes = STANDARD.decode(value).ok()?;
    if bytes.len() > 8192 {
        return None;
    }
    let data: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let url = reqwest::Url::parse(data.get("textures")?.get("SKIN")?.get("url")?.as_str()?).ok()?;
    if url.host_str()? != "textures.minecraft.net" || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let hash = url.path().strip_prefix("/texture/")?;
    if (32..=64).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(Text(hash.to_owned()))
    } else {
        None
    }
}
