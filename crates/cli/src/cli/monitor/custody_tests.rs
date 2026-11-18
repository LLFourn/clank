use super::*;

/// A run watch whose kernel wait fails is a failure the loop hears,
/// which ends the monitor, not silence.
#[test]
fn a_failed_run_watch_is_told_to_the_loop() {
    let (ended, mut runs_ended) = tokio::sync::mpsc::unbounded_channel();
    let exits = RunExits::new(ended);
    exits.watch.as_ref().unwrap().fail_for_test();
    let began = std::time::Instant::now();
    let heard = loop {
        match runs_ended.try_recv() {
            Ok(heard) => break Some(heard),
            Err(_) if began.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => break None,
        }
    };
    assert!(matches!(heard, Some(Err(_))), "{heard:?}");
}

/// A registered run's exit is an event: the loop is told the author,
/// and one already gone is told at once.
#[test]
fn a_run_that_exits_is_reported_without_being_looked_for() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let token = serde_json::to_string(&crate::proc_identity::token_for(pid).unwrap()).unwrap();
    let author = AgentLabel::parse("claude").unwrap();
    let proof = clank_core::turn_policy::ShellProof { pid, token };
    let (ended, mut runs_ended) = tokio::sync::mpsc::unbounded_channel();
    let mut exits = RunExits::new(ended);
    exits.track(vec![(author.clone(), proof.clone())]);
    exits.track(vec![(author.clone(), proof.clone())]);
    assert!(runs_ended.try_recv().is_err(), "it is running");
    child.kill().unwrap();
    child.wait().unwrap();
    let began = std::time::Instant::now();
    let heard = loop {
        match runs_ended.try_recv() {
            Ok(who) => break Some(who),
            Err(_) if began.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => break None,
        }
    };
    assert_eq!(heard, Some(Ok(author.clone())), "its exit");

    let (ended, mut runs_ended) = tokio::sync::mpsc::unbounded_channel();
    let mut later = RunExits::new(ended);
    later.track(vec![(author.clone(), proof.clone())]);
    assert_eq!(runs_ended.try_recv().ok(), Some(Ok(author)), "already gone");
    later.track(vec![(AgentLabel::parse("claude").unwrap(), proof)]);
    assert!(runs_ended.try_recv().is_err(), "and said once");
}
