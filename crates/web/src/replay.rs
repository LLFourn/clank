//! Retain the page's replay image from its small live frames. The repo
//! sends a full image only on attach or resync; every later publication
//! carries the frames that changed.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

#[derive(Default)]
struct Replay {
    panes: Option<String>,
    status: Option<String>,
    turns: BTreeMap<String, String>,
    screens: BTreeMap<String, String>,
}

impl Replay {
    fn from_frames(frames: &[String]) -> anyhow::Result<Self> {
        let mut replay = Self::default();
        for frame in frames {
            replay.apply(frame)?;
        }
        Ok(replay)
    }

    fn apply(&mut self, frame: &str) -> anyhow::Result<()> {
        let (event, data) = parse(frame)?;
        match event {
            "panes" => {
                let ids: BTreeSet<String> = data["panes"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("pane table is not an array"))?
                    .iter()
                    .filter_map(|pane| pane["id"].as_str().map(str::to_owned))
                    .collect();
                self.screens.retain(|id, _| ids.contains(id));
                self.panes = Some(frame.into());
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
                self.status = Some(frame.into());
            }
            "pane" => {
                let id = string(&data, "pane_id")?;
                self.screens.insert(id.into(), frame.into());
            }
            "turns" => {
                let agent = string(&data, "agent")?;
                self.turns.insert(agent.into(), frame.into());
            }
            "turn" => {
                let agent = string(&data, "agent")?;
                let Some(previous) = self.turns.get(agent) else {
                    return Ok(());
                };
                let (_, mut window) = parse(previous)?;
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
                self.turns.insert(agent.into(), sse("turns", &window));
            }
            _ => anyhow::bail!("unknown browser frame: {event}"),
        }
        Ok(())
    }

    fn frames(self) -> Vec<String> {
        let mut frames = Vec::new();
        if let Some(panes) = self.panes {
            frames.push(panes);
        }
        if let Some(status) = self.status {
            frames.push(status);
        }
        frames.extend(self.turns.into_values());
        frames.extend(self.screens.into_values());
        frames
    }
}

fn parse(frame: &str) -> anyhow::Result<(&str, Value)> {
    let (event, rest) = frame
        .split_once('\n')
        .ok_or_else(|| anyhow::anyhow!("frame has no data"))?;
    let event = event
        .strip_prefix("event: ")
        .ok_or_else(|| anyhow::anyhow!("frame has no event"))?;
    let data = rest
        .strip_prefix("data: ")
        .ok_or_else(|| anyhow::anyhow!("frame has no data field"))?;
    Ok((event, serde_json::from_str(data.trim_end_matches('\n'))?))
}

fn string<'a>(data: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    data[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("frame missing {field}"))
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
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
    let previous: Vec<String> = serde_json::from_value(
        previous
            .get("replay")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("browser replay missing"))?,
    )?;
    let live: Vec<String> = serde_json::from_value(live.clone())?;
    let mut replay = Replay::from_frames(&previous)?;
    for frame in &live {
        replay.apply(frame)?;
    }
    Ok(serde_json::json!({"replay":replay.frames(),"live":live}))
}

#[cfg(test)]
mod tests {
    use super::{merge, sse};

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
        assert!(changed["replay"][2].as_str().unwrap().contains("t1"));
        let newer = merge(
            &changed,
            &serde_json::json!({"live":[sse("status", &serde_json::json!({"phase":"second"}))]}),
        )
        .unwrap();
        assert!(newer["replay"][1].as_str().unwrap().contains("second"));
        assert!(!newer["replay"][1].as_str().unwrap().contains("first"));
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
