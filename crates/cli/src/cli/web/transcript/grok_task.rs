//! Grok's recorded task metadata. Registration is not command completion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BackgroundTask {
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

fn known_status(status: &str) -> bool {
    matches!(status, "running" | "completed" | "failed" | "cancelled")
}

/// Recognize only the provider's metadata prefix, not tags found in shell
/// output. Unknown/partial results retain their complete ordinary output.
fn parse(name: &str, input: &str, output: &str) -> Vec<BackgroundTask> {
    if name == "run_terminal_command" {
        let mut lines = output.lines();
        let mut field = |tag: &str| {
            lines
                .next()?
                .strip_prefix(&format!("<{tag}>"))?
                .strip_suffix(&format!("</{tag}>"))
                .map(str::to_owned)
        };
        let Some(id) = field("task-id") else {
            return vec![];
        };
        if field("task-type").as_deref() != Some("bash") || field("output-file").is_none() {
            return vec![];
        }
        let Some(status) = field("status") else {
            return vec![];
        };
        if field("summary").is_none()
            || !known_status(&status)
            || id.is_empty()
            || id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '<' || c == '>')
        {
            return vec![];
        }
        return vec![BackgroundTask {
            id,
            status,
            output: None,
        }];
    }
    if name != "get_command_or_subagent_output" {
        return vec![];
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(input) else {
        return vec![];
    };
    let Some(requested) = input.get("task_ids").and_then(|ids| ids.as_array()) else {
        return vec![];
    };
    let mut tasks = Vec::new();
    let mut sections = Vec::new();
    for (at, _) in output.match_indices("=== Task ") {
        if at == 0 || output.as_bytes()[at - 1] == b'\n' {
            sections.push(at);
        }
    }
    if sections.first() != Some(&0) {
        return vec![];
    }
    sections.push(output.len());
    for bounds in sections.windows(2) {
        let section = &output[bounds[0]..bounds[1]];
        let Some((header, body)) = section.split_once("\n\n") else {
            return vec![];
        };
        let mut lines = header.lines();
        let Some(id) = lines
            .next()
            .and_then(|line| line.strip_prefix("=== Task ")?.strip_suffix(" ==="))
        else {
            return vec![];
        };
        if !requested.iter().any(|value| value.as_str() == Some(id))
            || tasks.iter().any(|task: &BackgroundTask| task.id == id)
            || !lines
                .next()
                .is_some_and(|line| line.starts_with("Command: "))
        {
            return vec![];
        }
        let Some(status) = lines.next().and_then(|line| line.strip_prefix("Status: ")) else {
            return vec![];
        };
        if !known_status(status)
            || !lines
                .next()
                .is_some_and(|line| line.starts_with("Duration: "))
            || !lines
                .next()
                .is_some_and(|line| line.starts_with("Exit Code: "))
            || !lines
                .next()
                .is_some_and(|line| line.starts_with("Output File: "))
            || lines.next().is_some()
        {
            return vec![];
        }
        tasks.push(BackgroundTask {
            id: id.into(),
            status: status.into(),
            output: Some(body.trim_end_matches('\n').into()),
        });
    }
    tasks
}

/// Replay and incremental delivery use the same reducer and stable call IDs.
/// Terminal observations update the original registration too, without making
/// an old registration or duplicate running observation undo a terminal state.
pub(crate) fn apply(
    turns: &mut [super::Turn],
    id: &str,
    output: String,
    images: Vec<super::Image>,
) -> Vec<usize> {
    let Some(at) = turns.iter().position(|turn| turn.id == id) else {
        return vec![];
    };
    let terminal: Vec<_> = turns
        .iter()
        .flat_map(|turn| match &turn.body {
            super::Body::Tool { tasks, .. } => tasks.as_slice(),
            _ => &[],
        })
        .filter(|task| task.status != "running")
        .cloned()
        .collect();
    let super::Body::Tool {
        name,
        input,
        tasks,
        output: slot,
        images: shots,
        ..
    } = &mut turns[at].body
    else {
        return vec![];
    };
    let mut observed = parse(name, input, &output);
    for task in &mut observed {
        if let Some(previous) = terminal
            .iter()
            .rev()
            .find(|previous| previous.id == task.id)
            && previous.status != "running"
            && task.status == "running"
        {
            *task = previous.clone();
        }
    }
    *tasks = observed.clone();
    *slot = Some(output);
    *shots = images;
    let mut changed = vec![at];
    for (index, turn) in turns.iter_mut().enumerate() {
        if index == at {
            continue;
        }
        if let super::Body::Tool { tasks, .. } = &mut turn.body {
            let mut updated = false;
            for earlier in tasks {
                if earlier.status == "running"
                    && let Some(current) = observed
                        .iter()
                        .find(|task| task.id == earlier.id && task.status != "running")
                {
                    *earlier = current.clone();
                    updated = true;
                }
            }
            if updated {
                changed.push(index);
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::web::transcript::{Body, Parsed, parse_at};
    use clank_core::vocab::Tool;
    const TASK: &str = "01a103bf-f8fc-7af2-8aae-698aebcc7c35";
    const REGISTERED: &str = "<task-id>01a103bf-f8fc-7af2-8aae-698aebcc7c35</task-id>\n<task-type>bash</task-type>\n<output-file>/private/terminal/call-1/157.log</output-file>\n<status>running</status>\n<summary>Background task started</summary>\nUse get_command or subagent output when you need the output.";
    fn observation(id: &str, status: &str, output: &str) -> String {
        format!(
            "=== Task {id} ===\nCommand: clank wait\nStatus: {status}\nDuration: 1s\nExit Code: 0\nOutput File: /private/terminal/call-1/157.log\n\n{output}\n"
        )
    }
    #[test]
    fn actual_registration_and_output_shapes_update_without_confusing_completion() {
        let record = serde_json::json!({"type":"assistant","tool_calls":[
            {"id":"call-1","name":"run_terminal_command","arguments":{"command":"clank wait","description":"Arm one automatic Grok handoff wait","block_until_ms":0}},
            {"id":"call-2","name":"get_command_or_subagent_output","arguments":{"task_ids":[TASK]}}]}).to_string();
        let mut turns: Vec<_> = parse_at(Tool::Grok, &record, 100)
            .into_iter()
            .filter_map(|item| match item {
                Parsed::Turn(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            matches!(&turns[0].body, Body::Tool {title,detail,..} if title=="clank wait" && detail=="clank wait")
        );
        let result =
            serde_json::json!({"type":"tool_result","tool_call_id":"call-1","content":REGISTERED})
                .to_string();
        let Parsed::ToolOutput { id, output, images } =
            parse_at(Tool::Grok, &result, 400).remove(0)
        else {
            panic!("result")
        };
        assert_eq!(apply(&mut turns, &id, output, images), vec![0]);
        assert!(
            matches!(&turns[0].body,Body::Tool{tasks,output:Some(raw),..} if tasks[0].status=="running" && raw==REGISTERED)
        );
        assert_eq!(
            apply(
                &mut turns,
                "grok-tool:call-2",
                observation(TASK, "completed", "actual output"),
                vec![]
            ),
            vec![1, 0]
        );
        assert!(
            matches!(&turns[0].body,Body::Tool{tasks,output:Some(raw),..} if tasks[0].status=="completed" && tasks[0].output.as_deref()==Some("actual output") && raw==REGISTERED)
        );
        apply(&mut turns, "grok-tool:call-1", REGISTERED.into(), vec![]);
        assert!(
            matches!(&turns[0].body,Body::Tool{tasks,..} if tasks[0].status=="completed"),
            "repeated registration cannot undo completion"
        );
        assert_eq!(turns[0].id, "grok-tool:call-1");
    }
    #[test]
    fn several_observations_match_requested_tasks_and_keep_shell_output() {
        let input = serde_json::json!({"task_ids":["a","b"]}).to_string();
        let output = observation("a", "failed", "failure output")
            + &observation("b", "cancelled", "cancelled output");
        let tasks = parse("get_command_or_subagent_output", &input, &output);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, "failed");
        assert_eq!(tasks[1].status, "cancelled");
        assert!(
            parse(
                "get_command_or_subagent_output",
                &input,
                &observation("other", "completed", "anything")
            )
            .is_empty()
        );
        assert!(
            parse(
                "get_command_or_subagent_output",
                &input,
                &(observation("a", "completed", "first")
                    + &observation("a", "running", "duplicate"))
            )
            .is_empty()
        );
    }
    #[test]
    fn unknown_partial_and_unrelated_payloads_keep_plain_fallback() {
        for output in [
            "normal output",
            "<status>running</status>\n<task-id>x</task-id>",
            "<task-id>partial</task-id>",
        ] {
            assert!(parse("run_terminal_command", "{}", output).is_empty());
        }
        assert!(parse("read_file", "{}", REGISTERED).is_empty());
        assert!(
            parse(
                "run_terminal_command",
                "{}",
                &REGISTERED.replace("running", "future")
            )
            .is_empty()
        );
        for status in ["failed", "cancelled", "completed"] {
            assert_eq!(
                parse(
                    "run_terminal_command",
                    "{}",
                    &REGISTERED.replace("running", status)
                )[0]
                .status,
                status
            );
        }
    }
}
