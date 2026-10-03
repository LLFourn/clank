//! Session transport shared by the Clank TUI connector and web hosts.
//!
//! Hosts provide their own browser and connector admission. This crate
//! handles the ordered session connection and request outcomes.

pub mod assets;
pub mod bridge;
mod bridge_codec;
mod bridge_transport;
mod replay;
pub mod request;
pub mod service;
