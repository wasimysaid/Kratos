//! Compiles the active browser lifecycle and connection modules without GPUI or a native engine runtime.

#[path = "../../src/rpc/connection.rs"]
pub mod browser_connection;
#[cfg(not(target_arch = "wasm32"))]
#[path = "../../src/browser_session.rs"]
pub mod browser_session;
#[path = "../../../../crates/ui/src/state/connection.rs"]
pub mod engine_connection;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod connection_tests;
