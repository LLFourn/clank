//! Browser operations carried to the repo connector. The host checks
//! admission and request size; the repo adapter revalidates targets and
//! resource revisions against current local state before acting.

use serde::{Deserialize, Serialize};

use crate::bridge::Action;

pub const ACTION_KIND: &str = "browser_request";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BrowserRequest {
    Say {
        pane: String,
        label: String,
        session: Option<String>,
        text: String,
    },
    Stop {
        pane: String,
        label: String,
        session: Option<String>,
    },
    Queue {
        ask: serde_json::Value,
    },
    Stash {
        ask: serde_json::Value,
    },
    Agent {
        ask: serde_json::Value,
    },
    AgentCandidates,
    BlockAnswer {
        agent: String,
        name: String,
        question: String,
        answer: String,
    },
    Reopen {
        label: String,
    },
    Site {
        path: String,
    },
}

impl BrowserRequest {
    pub fn into_action(self) -> Action {
        Action {
            kind: ACTION_KIND.into(),
            input: serde_json::to_value(self).expect("browser request serializes"),
        }
    }

    pub fn from_action(action: Action) -> anyhow::Result<Self> {
        anyhow::ensure!(action.kind == ACTION_KIND, "unsupported action kind");
        Ok(serde_json::from_value(action.input)?)
    }
}

/// The repo's HTTP result. The browser service preserves status and body
/// instead of treating a successful bridge delivery as an agent receipt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserReply {
    pub status: u16,
    pub content: Content,
    pub body: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Plain,
    Json,
    Html,
    Css,
}

impl Content {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Plain => "text/plain; charset=utf-8",
            Self::Json => "application/json",
            Self::Html => "text/html; charset=utf-8",
            Self::Css => "text/css; charset=utf-8",
        }
    }
}

impl BrowserReply {
    pub fn into_value(self) -> serde_json::Value {
        serde_json::to_value(self).expect("browser reply serializes")
    }

    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let reply: Self = serde_json::from_value(value)?;
        anyhow::ensure!(
            (200..600).contains(&reply.status),
            "invalid browser response status"
        );
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserReply, BrowserRequest, Content};

    #[test]
    fn request_round_trip_keeps_the_target_and_revision() {
        let request = BrowserRequest::Queue {
            ask: serde_json::json!({"do":"save","handle":"my-plan","revision":"old","body":"new"}),
        };
        assert_eq!(
            BrowserRequest::from_action(request.clone().into_action()).unwrap(),
            request
        );
        let response = BrowserReply {
            status: 409,
            content: Content::Json,
            body: r#"{"why":"stale revision"}"#.into(),
        };
        assert_eq!(
            BrowserReply::from_value(response.clone().into_value()).unwrap(),
            response
        );
    }
}
