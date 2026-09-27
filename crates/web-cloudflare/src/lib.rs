//! Components for the default Clank web host.
//!
//! The host uses `clank-web` for the session protocol and browser UI.
//! Its login assets and configuration are deliberately absent from the
//! embeddable crate. The server process and tunnel live here.

pub const LOGIN: &str = include_str!("../assets/login.html");
pub const DOOR_JS: &str = include_str!("../assets/door.js");

pub mod config;
pub mod dns;
pub mod door;
pub mod host;
pub mod origin;
pub mod process;
pub mod tunnel;
