//! Cross-shard messaging, presence, and channels.
//!
//! Design: docs/PROPOSAL.md ("Service / Crate Breakdown"), docs/specs/Chat_Spec.md,
//! and [Decision #82](https://github.com/LunarVagabond/WorldZero/issues/82).
//! Presence isn't implemented yet — see docs/specs/Chat_Spec.md, "Not this pass".

pub mod demo_support;
pub mod gateway_protocol;
pub mod message_log;
pub mod pubsub;
pub mod schema;
pub mod store;

pub use message_log::{MessageLog, persistence_enabled, persistence_enabled_from_env};
pub use pubsub::{ChatBus, ChatMessage};
pub use schema::SystemChannelConfig;
pub use store::{ChannelStore, ChannelType};
