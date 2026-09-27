//! Configuration owned by the default Clank web host.

use serde::{Deserialize, Serialize};

/// `~/.clank/config.json#/remote`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<TunnelSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// `~/.clank/config.json#/remote/tunnel`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "kebab-case")]
pub enum TunnelSection {
    Quick,
    /// Retained so older user configurations can be parsed and given
    /// the existing migration advice.
    Ngrok {
        domain: String,
    },
    Command {
        run: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url_contains: Option<String>,
    },
}
