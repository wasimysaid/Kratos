//! Durable-peer sync clients and storage.
//!
//! - [`ChatClient`] implements the chat2 row/checkpoint protocol over the managed peer.
//! - [`RegistryClient`] synchronizes profile-scoped registry rows and presence.
//! - [`peer`] serves authenticated durable rows, checkpoints, sidecars, attachments, previews,
//!   and targeted device links from the headless peer.
//! - [`DocsStore`] persists local snapshots, pending update outboxes, and the
//!   processed-command ledger with mark-before-execute semantics.

pub mod chat_client;
pub mod chat_frames;
pub mod dial;
pub mod net_path;

pub mod peer;
pub mod registry;
pub mod socket;
mod store;
mod types;
pub mod wake;

pub use chat_client::{
    ChatClient, ChatDocSink, ChatEvent, ChatStatsSnapshot, ChatTuning, CheckpointFetcher,
};
pub use registry::{
    ReconnectState, RegistryClient, RegistryEvent, RegistryTransport, RegistryTuning,
};
pub use store::{DocsStore, StoreError};
pub use types::{RoomStatsSnapshot, StaticUrl, SyncError, UrlProvider};
