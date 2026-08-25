//! Strongly-typed IDs: a UUID wrapped with a phantom domain marker so
//! e.g. `CharacterId` and `RealmId` are distinct types, not interchangeable
//! raw UUIDs.
//!
//! ```
//! use common::id::Id;
//!
//! struct WidgetMarker;
//! type WidgetId = Id<WidgetMarker>;
//!
//! let id = WidgetId::new();
//! assert_eq!(id, WidgetId::from_uuid(id.as_uuid()));
//! ```
//!
//! Mixing two domains is a compile error:
//!
//! ```compile_fail
//! use common::id::{CharacterId, RealmId};
//!
//! fn takes_realm(_: RealmId) {}
//!
//! takes_realm(CharacterId::new());
//! ```

use std::any::type_name;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

pub struct Id<T> {
    uuid: Uuid,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    pub fn new() -> Self {
        Self {
            uuid: Uuid::now_v7(),
            _marker: PhantomData,
        }
    }

    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            _marker: PhantomData,
        }
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.uuid
    }
}

impl<T> Default for Id<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl<T> Eq for Id<T> {}

impl<T> Hash for Id<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.uuid.cmp(&other.uuid)
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let domain = type_name::<T>().rsplit("::").next().unwrap_or("Id");
        write!(f, "{domain}({})", self.uuid)
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.uuid, f)
    }
}

impl<T> FromStr for Id<T> {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_uuid(Uuid::from_str(s)?))
    }
}

impl<T> Serialize for Id<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.uuid.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_uuid(Uuid::deserialize(deserializer)?))
    }
}

pub struct CharacterMarker;
pub type CharacterId = Id<CharacterMarker>;

pub struct RealmMarker;
pub type RealmId = Id<RealmMarker>;

pub struct ZoneMarker;
pub type ZoneId = Id<ZoneMarker>;

pub struct AccountMarker;
pub type AccountId = Id<AccountMarker>;

/// A generic simulated entity — covers players, NPCs, and items alike
/// (docs/PROPOSAL.md, "v0 Hooks": "Generic entity model (covers players,
/// NPCs, items)"). Not the same id as `CharacterId`: a player's `world`
/// entity and their durable `character` record are related but distinct
/// concerns — the entity exists only while simulated in a zone, the
/// character record persists whether or not anyone's logged in.
pub struct EntityMarker;
pub type EntityId = Id<EntityMarker>;

pub struct ChannelMarker;
pub type ChannelId = Id<ChannelMarker>;

/// A party/group's durable identity (#178) — a small roster of
/// `CharacterId`s, not `AccountId`s (a party membership is per-character,
/// matching `#142`'s reconnect-placement logic, which already keys off
/// the specific character too).
pub struct PartyMarker;
pub type PartyId = Id<PartyMarker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_ids_are_not_equal() {
        let a = CharacterId::new();
        let b = CharacterId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn round_trips_through_uuid() {
        let id = RealmId::new();
        assert_eq!(RealmId::from_uuid(id.as_uuid()), id);
    }

    #[test]
    fn round_trips_through_display_and_from_str() {
        let id = ZoneId::new();
        let parsed: ZoneId = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn round_trips_through_json() {
        let id = AccountId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: AccountId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn usable_as_a_map_key() {
        let mut map = std::collections::HashMap::new();
        let id = CharacterId::new();
        map.insert(id, "hero");
        assert_eq!(map.get(&id), Some(&"hero"));
    }

    #[test]
    fn debug_shows_the_domain_name() {
        let id = CharacterId::new();
        assert!(format!("{id:?}").starts_with("CharacterMarker("));
    }
}
