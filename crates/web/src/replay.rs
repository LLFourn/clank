//! Retain the page's replay image from its small live frames. The repo
//! sends a full image only on attach or resync; every later publication
//! carries the frames that changed.

use std::collections::{BTreeMap, BTreeSet};

use crate::service::BrowserFrame;
use serde_json::Value;

#[derive(Default)]
struct Replay {
    panes: Option<BrowserFrame>,
    status: Option<BrowserFrame>,
    questions: Option<BrowserFrame>,
    turns: BTreeMap<String, BrowserFrame>,
    screens: BTreeMap<String, BrowserFrame>,
}

impl Replay {
    fn from_frames(frames: &[BrowserFrame]) -> anyhow::Result<Self> {
        let mut replay = Self::default();
        for frame in frames {
            replay.apply(frame)?;
        }
        Ok(replay)
    }

    fn apply(&mut self, frame: &BrowserFrame) -> anyhow::Result<()> {
        let event = frame.event.as_str();
        let data = &frame.data;
        match event {
            "panes" => {
                let ids: BTreeSet<String> = data["panes"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("pane table is not an array"))?
                    .iter()
                    .filter_map(|pane| pane["id"].as_str().map(str::to_owned))
                    .collect();
                self.screens.retain(|id, _| ids.contains(id));
                self.panes = Some(frame.clone());
            }
            "status" => {
                if let Some(agents) = data.pointer("/facts/agents").and_then(Value::as_array) {
                    let labels: BTreeSet<&str> = agents
                        .iter()
                        .filter_map(|agent| agent["label"].as_str())
                        .collect();
                    self.turns
                        .retain(|label, _| labels.contains(label.as_str()));
                }
                self.status = Some(frame.clone());
            }
            "questions" => {
                anyhow::ensure!(
                    data["agents"].is_object(),
                    "question windows must be an object"
                );
                self.questions = Some(frame.clone());
            }
            "pane" => {
                let id = string(data, "pane_id")?;
                self.screens.insert(id.into(), frame.clone());
            }
            "turns" => {
                let agent = string(data, "agent")?;
                self.turns.insert(agent.into(), frame.clone());
            }
            "turn" => {
                let agent = string(data, "agent")?;
                let Some(previous) = self.turns.get(agent) else {
                    return Ok(());
                };
                let mut window = previous.data.clone();
                if window["session"] != data["session"]
                    || window["generation"] != data["generation"]
                {
                    return Ok(());
                }
                let turn = data
                    .get("turn")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("turn missing"))?;
                let id = string(&turn, "id")?;
                let list = window["turns"]
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("turn window missing"))?;
                if let Some(old) = list.iter_mut().find(|item| item["id"] == id) {
                    *old = turn;
                } else {
                    list.push(turn);
                }
                if list.len() > 200 {
                    list.drain(..list.len() - 200);
                }
                self.turns
                    .insert(agent.into(), BrowserFrame::new("turns", window));
            }
            _ => anyhow::bail!("unknown browser frame: {event}"),
        }
        Ok(())
    }

    fn frames(self) -> Vec<BrowserFrame> {
        let mut frames = Vec::new();
        if let Some(panes) = self.panes {
            frames.push(panes);
        }
        if let Some(status) = self.status {
            frames.push(status);
        }
        frames.extend(self.turns.into_values());
        if let Some(questions) = self.questions {
            frames.push(questions);
        }
        frames.extend(self.screens.into_values());
        frames
    }
}

fn string<'a>(data: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    data[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("frame missing {field}"))
}

/// Merge a live-frame delta into a retained browser replay. Other JSON
/// state keeps the protocol's original full-replacement semantics.
pub(crate) fn merge(previous: &Value, change: &Value) -> anyhow::Result<Value> {
    let Some(live) = change.get("live") else {
        return Ok(change.clone());
    };
    if change.get("replay").is_some() {
        return Ok(change.clone());
    }
    let previous: Vec<BrowserFrame> = serde_json::from_value(
        previous
            .get("replay")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("browser replay missing"))?,
    )?;
    let live: Vec<BrowserFrame> = serde_json::from_value(live.clone())?;
    let mut replay = Replay::from_frames(&previous)?;
    for frame in &live {
        replay.apply(frame)?;
    }
    Ok(serde_json::json!({"replay":replay.frames(),"live":live}))
}

#[cfg(test)]
mod tests {
    use super::merge;
    fn sse(event: &str, data: &serde_json::Value) -> super::BrowserFrame {
        super::BrowserFrame::new(event, data.clone())
    }

    #[test]
    fn questions_replace_the_native_window_and_empty_images_clear_it() {
        let old = serde_json::json!({"replay":[sse("questions", &serde_json::json!({"agents":{
            "codex":{"session":"one","owner":"old","pending":[{"id":7}]}}}))],"live":[]});
        let replacement = sse(
            "questions",
            &serde_json::json!({"agents":{
            "codex":{"session":"two","owner":"new","pending":[{"id":7}]}}}),
        );
        let updated = merge(&old, &serde_json::json!({"live":[replacement.clone()]})).unwrap();
        assert_eq!(
            updated["replay"][0],
            serde_json::to_value(replacement).unwrap()
        );
        let cleared = sse("questions", &serde_json::json!({"agents":{}}));
        let updated = merge(&updated, &serde_json::json!({"live":[cleared.clone()]})).unwrap();
        assert_eq!(updated["replay"][0], serde_json::to_value(cleared).unwrap());
        assert!(
            merge(
                &updated,
                &serde_json::json!({"live":[sse("questions", &serde_json::json!({"agents":[]}))]})
            )
            .is_err()
        );
    }

    #[test]
    fn live_frames_update_the_retained_image_without_replacing_it() {
        let panes = sse(
            "panes",
            &serde_json::json!({"panes":[{"id":"p1"}],"closed":[]}),
        );
        let status = sse(
            "status",
            &serde_json::json!({"phase":"first","facts":{"agents":[{"label":"a"}]}}),
        );
        let turns = sse(
            "turns",
            &serde_json::json!({"agent":"a","session":"s1","generation":1,"turns":[]}),
        );
        let screen = sse(
            "pane",
            &serde_json::json!({"pane_id":"p1","screen":"hello"}),
        );
        let start = serde_json::json!({"replay":[panes,status,turns,screen],"live":[]});
        let turn = sse(
            "turn",
            &serde_json::json!({"agent":"a","session":"s1","generation":1,"turn":{"id":"t1","who":"person"}}),
        );
        let changed = merge(&start, &serde_json::json!({"live":[turn]})).unwrap();
        assert_eq!(changed["replay"].as_array().unwrap().len(), 4);
        assert!(changed["replay"][2]["data"]["turns"][0]["id"] == "t1");
        let newer = merge(
            &changed,
            &serde_json::json!({"live":[sse("status", &serde_json::json!({"phase":"second"}))]}),
        )
        .unwrap();
        assert!(newer["replay"][1]["data"]["phase"] == "second");
        assert_ne!(newer["replay"][1]["data"]["phase"], "first");
        let removed = merge(
            &newer,
            &serde_json::json!({"live":[
                sse("panes", &serde_json::json!({"panes":[],"closed":[]})),
                sse("status", &serde_json::json!({"facts":{"agents":[]}})),
            ]}),
        )
        .unwrap();
        assert_eq!(removed["replay"].as_array().unwrap().len(), 2);
    }
}
