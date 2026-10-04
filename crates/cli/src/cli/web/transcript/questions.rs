//! Recorded question content, never evidence of a live answer channel.
//!
//! The transcript call id correlates the recorded result; it is not the
//! provider's pending RPC request id. Keep parsing here so the browser
//! renders typed fields without guessing tool names or JSON input shapes.

use clank_core::vocab::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Question {
    pub(crate) provider: Provider,
    pub(crate) fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Field {
    /// Codex's field id. Claude answers are keyed by the exact question text.
    pub(crate) id: Option<String>,
    pub(crate) header: String,
    pub(crate) question: String,
    pub(crate) multi_select: bool,
    pub(crate) is_secret: bool,
    pub(crate) is_other: bool,
    pub(crate) options: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Choice {
    pub(crate) label: String,
    pub(crate) description: String,
}

/// Recognize only native question calls from their own harness. If any
/// field is malformed, leave the original tool call intact for inspection.
/// No truncation or invented limits: the original tool input is retained too.
pub(crate) fn parse(tool: Tool, name: &str, input: &str) -> Option<Question> {
    let (provider, asynchronous) = match (tool, name) {
        (Tool::Claude, "AskUserQuestion") => (Provider::Claude, false),
        (Tool::Codex, "request_user_input" | "functions.request_user_input") => {
            (Provider::Codex, false)
        }
        (Tool::Codex, "request_user_input_async" | "functions.request_user_input_async") => {
            (Provider::Codex, true)
        }
        _ => return None,
    };
    let input: Value = serde_json::from_str(input).ok()?;
    let fields = input.get("questions")?.as_array()?;
    if fields.is_empty() {
        return None;
    }
    let fields = fields
        .iter()
        .map(|field| {
            let question = field
                .get(if asynchronous { "title" } else { "question" })?
                .as_str()?;
            if question.trim().is_empty() {
                return None;
            }
            let text = |key: &str| -> Option<String> {
                match field.get(key) {
                    None => Some(String::new()),
                    Some(value) => Some(value.as_str()?.to_owned()),
                }
            };
            let id = match field.get("id") {
                None => None,
                Some(value) => Some(value.as_str()?.to_owned()),
            };
            let multi_select = match field.get("multiSelect") {
                None => false,
                Some(value) => value.as_bool()?,
            };
            let flag = |camel: &str, snake: &str| -> Option<bool> {
                match field.get(camel).or_else(|| field.get(snake)) {
                    None => Some(false),
                    Some(value) => value.as_bool(),
                }
            };
            let options = match field.get("options") {
                None | Some(Value::Null) if provider == Provider::Codex => Vec::new(),
                Some(value) => value
                    .as_array()?
                    .iter()
                    .map(|option| {
                        if asynchronous {
                            return Some(Choice {
                                label: option.as_str()?.to_owned(),
                                description: String::new(),
                            });
                        }
                        Some(Choice {
                            label: option.get("label")?.as_str()?.to_owned(),
                            description: match option.get("description") {
                                None => String::new(),
                                Some(value) => value.as_str()?.to_owned(),
                            },
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            Some(Field {
                id,
                header: text("header")?,
                question: question.to_owned(),
                multi_select,
                is_secret: flag("isSecret", "is_secret")?,
                is_other: flag("isOther", "is_other")?,
                options,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Question { provider, fields })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_keeps_exact_question_keys_multiple_selection_and_option_descriptions() {
        let input = json!({"questions":[{"question":"Choose <a> & <b>?", "header":"Packages",
            "multiSelect":true, "options":[{"label":"<a>","description":"First"},
                {"label":"<b>","description":"Second"}]}]})
        .to_string();
        let q = parse(Tool::Claude, "AskUserQuestion", &input).unwrap();
        assert_eq!(q.provider, Provider::Claude);
        assert_eq!(q.fields[0].question, "Choose <a> & <b>?");
        assert!(q.fields[0].multi_select);
        assert_eq!(q.fields[0].id, None);
        assert_eq!(q.fields[0].options[1].description, "Second");
    }

    #[test]
    fn codex_keeps_field_ids_and_supports_background_free_text_questions() {
        let input = json!({"questions":[{"id":"language", "header":"Language",
            "question":"Which language?", "options":[{"label":"Rust","description":"Native"}]}]})
        .to_string();
        for name in ["request_user_input", "functions.request_user_input"] {
            let q = parse(Tool::Codex, name, &input).unwrap();
            assert_eq!(q.provider, Provider::Codex);
            assert_eq!(q.fields[0].id.as_deref(), Some("language"));
        }
        let input = json!({"questions":[{"title":"Which option?", "options":["A","B"]},
            {"title":"Additional details?"}]})
        .to_string();
        let q = parse(Tool::Codex, "functions.request_user_input_async", &input).unwrap();
        assert_eq!(q.fields.len(), 2);
        assert_eq!(q.fields[0].options[1].label, "B");
        assert!(q.fields[1].options.is_empty());
        let input = json!({"questions":[{"id":"secret", "header":"Token", "question":"Your token?",
            "options":null, "isSecret":true, "isOther":true}]})
        .to_string();
        let q = parse(Tool::Codex, "request_user_input", &input).unwrap();
        assert!(q.fields[0].options.is_empty());
        assert!(q.fields[0].is_secret && q.fields[0].is_other);
    }

    #[test]
    fn unrelated_and_malformed_calls_stay_generic_tools_without_losing_their_input() {
        let valid = json!({"questions":[{"question":"Q?","options":[]}]}).to_string();
        for (tool, name) in [
            (Tool::Grok, "AskUserQuestion"),
            (Tool::Claude, "request_user_input"),
            (Tool::Codex, "some_request_user_input"),
        ] {
            assert!(parse(tool, name, &valid).is_none());
        }
        for input in [
            "not json",
            "{}",
            r#"{"questions":[]}"#,
            r#"{"questions":[{"question":"","options":[] }]}"#,
            r#"{"questions":[{"question":"Q?","options":[null]}]}"#,
            r#"{"questions":[{"question":"Q?","multiSelect":"yes","options":[]}]}"#,
        ] {
            assert!(parse(Tool::Claude, "AskUserQuestion", input).is_none());
        }
        let input =
            json!({"questions":[{"question":"long".repeat(5000),"options":[]}]}).to_string();
        assert_eq!(
            parse(Tool::Claude, "AskUserQuestion", &input)
                .unwrap()
                .fields[0]
                .question
                .len(),
            20_000
        );
    }
}
