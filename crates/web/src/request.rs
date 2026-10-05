//! Browser operations carried to the repo connector. The host checks
//! admission and request size; the repo adapter revalidates targets and
//! resource revisions against current local state before acting.

use serde::{Deserialize, Serialize};

use crate::bridge::Action;

pub const ACTION_KIND: &str = "browser_request";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BrowserRequest {
    TranscriptImage {
        ask: TranscriptImage,
    },
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
    QuestionReconnect {
        ask: QuestionReconnect,
    },
    QuestionAnswer {
        ask: QuestionAnswer,
    },
    Reopen {
        label: String,
    },
    Site {
        path: String,
    },
}

/// A retained attachment, never a filesystem path. The window and content key
/// prevent a delayed request from returning an image from a replacement turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TranscriptImage {
    pub agent: String,
    pub session: String,
    pub generation: u32,
    pub turn: String,
    pub index: usize,
    pub key: String,
    pub preview: bool,
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    fn attachment_requests_roundtrip_without_any_filesystem_authority() {
        let value = serde_json::json!({"agent":"a","session":"s","generation":1,"turn":"t","index":0,"key":"content","preview":true});
        let ask: TranscriptImage = serde_json::from_value(value.clone()).unwrap();
        let request = BrowserRequest::TranscriptImage { ask };
        assert_eq!(
            BrowserRequest::from_action(request.clone().into_action()).unwrap(),
            request
        );
        let mut path = value;
        path["path"] = serde_json::json!("/private/file.png");
        assert!(serde_json::from_value::<TranscriptImage>(path).is_err());
    }
}

/// Recovery targets the existing owner, never a transcript or a new session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuestionReconnect {
    pub agent: String,
    pub session: String,
    pub owner: String,
}

/// Exact native authority plus a client attempt identity. Transcript IDs and
/// a successfully delivered HTTP request are not native answer receipts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub agent: String,
    pub session: String,
    pub owner: String,
    /// JSON-encoded native ID, opaque to browsers to preserve u64 precision.
    pub id: String,
    pub turn: String,
    pub item: String,
    pub attempt: String,
    pub answers: std::collections::BTreeMap<String, Vec<String>>,
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
