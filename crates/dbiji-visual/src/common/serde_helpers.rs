//! Serde adapters for egui math types (egui is used without its `serde`
//! feature). Points and vectors are stored as `[x, y]`; non-finite numbers are
//! written as `0` so documents stay valid JSON.

use egui::{Pos2, Vec2};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// `Pos2` as `[x, y]`.
pub(crate) mod pos2 {
    use super::*;

    pub fn serialize<S: Serializer>(p: &Pos2, s: S) -> Result<S::Ok, S::Error> {
        [finite(p.x), finite(p.y)].serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pos2, D::Error> {
        let [x, y] = <[f32; 2]>::deserialize(d)?;
        Ok(Pos2::new(x, y))
    }
}

/// `Vec2` as `[x, y]`.
pub(crate) mod vec2 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Vec2, s: S) -> Result<S::Ok, S::Error> {
        [finite(v.x), finite(v.y)].serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec2, D::Error> {
        let [x, y] = <[f32; 2]>::deserialize(d)?;
        Ok(Vec2::new(x, y))
    }
}

/// `BTreeMap<String, Pos2>` as a map of `[x, y]`.
pub(crate) mod pos2_map {
    use super::*;

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, Pos2>, s: S) -> Result<S::Ok, S::Error> {
        let plain: BTreeMap<&str, [f32; 2]> = m
            .iter()
            .map(|(k, p)| (k.as_str(), [finite(p.x), finite(p.y)]))
            .collect();
        plain.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Pos2>, D::Error> {
        let plain = BTreeMap::<String, [f32; 2]>::deserialize(d)?;
        Ok(plain
            .into_iter()
            .map(|(k, [x, y])| (k, Pos2::new(x, y)))
            .collect())
    }
}
