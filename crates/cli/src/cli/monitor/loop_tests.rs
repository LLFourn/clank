/// The loop has no tick: each arm it waits on is a channel, a
/// notification, or one of its own deadlines, each computed from
/// state it holds. Read from the source, since no test may run a
/// monitor: a heartbeat restored as an arm fails here, and an idle
/// repo with no deadline therefore wakes nothing — no fold, no
/// serve, no probe, no note to any client.
/// Before it sleeps the loop waits for the release of every lock its
/// pass found held, and no deadline of its own stands for one; on a
/// release, as owner, it runs [`retry_contended`] and recovery. Read
/// from the source, since no test may run a monitor.
#[test]
fn the_loop_waits_for_held_locks_releases_and_retries_on_one() {
    let src = include_str!("mod.rs");
    let body = &src[src.find("    let ended = loop {").expect("the loop")..];
    let armed = body
        .find("        releases.arm()?;\n")
        .expect("the loop arms");
    let select = body
        .find("        let wake = tokio::select! {")
        .expect("the select");
    assert!(armed < select, "armed before the loop sleeps");
    let arms = &body[select..select + body[select..].find("\n        };\n").unwrap()];
    assert!(
        arms.contains("Some(_) = releases.released() => Wake::Released,"),
        "{arms}"
    );
    let arm = &body[body
        .find("            Wake::Released => {")
        .expect("the arm")..];
    let arm = &arm[..arm.find("            Wake::RunEnded(").expect("its end")];
    let owner = arm.find("if owner {").expect("only an owner retries");
    let released = arm.find("released(").expect("the loop runs the release");
    assert!(owner < released, "{arm}");
    let released = item(src, "fn released(");
    let ran = released.find("retry_contended(").expect("the retry runs");
    let looked = released
        .find("custody.released(")
        .expect("recovery looks again");
    assert!(ran < looked, "{released}");
    let retried = item(src, "fn retry_contended(");
    assert!(
        retried.contains("clients.present(") && retried.contains("clients.serve_reports("),
        "the waits and reports are served again by the clients, which keep the retired \
         ones' writers: {retried}"
    );
    let custody = method(include_str!("custody.rs"), "pub(super) fn released(");
    let facts = custody
        .find("recovery_facts(")
        .expect("recovery folds again");
    assert!(
        custody[..facts]
            .trim_end()
            .ends_with("let acts = self.recovery.released("),
        "the fold is recovery's release, which owes a look a lock held: {custody}"
    );
}

/// The body of the free function `head` begins, through its closing brace.
fn item<'a>(src: &'a str, head: &str) -> &'a str {
    let from = src.find(head).unwrap_or_else(|| panic!("{head}"));
    &src[from..from + src[from..].find("\n}\n").expect("its end")]
}

/// The body of the item `head` begins, through its closing brace at the
/// indentation of a method.
fn method<'a>(src: &'a str, head: &str) -> &'a str {
    let from = src.find(head).unwrap_or_else(|| panic!("{head}"));
    &src[from..from + src[from..].find("\n    }\n").expect("its end")]
}

/// A monitor that cannot watch its installation does not serve, and one
/// whose watch fails ends, saying why: it would otherwise serve on past
/// the build installed to replace it. Read, since no test may run one.
#[test]
fn a_monitor_deaf_to_installs_ends() {
    let flat = |src: &str| -> String { src.split_whitespace().collect() };
    let lease = include_str!("lease.rs");
    assert!(
        flat(method(lease, "pub(super) fn take("))
            .contains(".map_err(|why|anyhow::anyhow!(\"cannotwatchtheinstallation:{why}\"))?;")
    );
    assert!(flat(include_str!("mod.rs")).contains("Wake::Installed=>{hold.installed()?;"));
    assert!(flat(method(lease, "pub(super) fn installed(")).contains(
        "ifletSome(why)=self.installs_lost.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take(){anyhow::bail!(\"thewatchontheinstallationfailed:{why}\");}"
    ));
}

/// An install is what a decision a stale build could not store waits
/// on, so the loop's install wake hands it to recovery, as owner, and
/// acts on what that decides. Read, since no test may run a monitor.
#[test]
fn an_install_wake_reaches_recovery() {
    let src = include_str!("mod.rs");
    let body = &src[src.find("    let ended = loop {").expect("the loop")..];
    let select = body
        .find("        let wake = tokio::select! {")
        .expect("the select");
    let arms = &body[select..select + body[select..].find("\n        };\n").unwrap()];
    assert!(
        arms.contains("_ = hold.installs().notified() => Wake::Installed,"),
        "{arms}"
    );
    let arm = &body[body
        .find("            Wake::Installed => {")
        .expect("the arm")..];
    let arm = &arm[..arm.find("            Wake::LeaseChanged").expect("its end")];
    let flat = |src: &str| -> String { src.split_whitespace().collect() };
    assert!(
        flat(arm).contains("ifowner{custody.installed(&repo,&clients,&mutowned);}"),
        "{arm}"
    );
    let custody = include_str!("custody.rs");
    let installed = method(custody, "pub(super) fn installed(");
    assert!(
        flat(installed).contains(
            "{letacts=self.recovery.installed(crate::cli::recovery::unix_now());self.act(repo,clients,acts,owned);"
        ),
        "{installed}"
    );
    let act = method(custody, "pub(super) fn act(");
    assert!(
        flat(act).contains("{act_on_recovery(repo,clients,acts,"),
        "{act}"
    );
}

#[test]
fn the_loop_waits_only_on_events_and_its_own_deadlines() {
    let src = include_str!("mod.rs");
    let from = src
        .find("        let wake = tokio::select! {")
        .expect("the loop's select");
    let arms = &src[from..from + src[from..].find("\n        };\n").expect("its end")];
    for poll in ["sleep(", "interval", "Beat::next", "tick"] {
        assert!(!arms.contains(poll), "{poll} in the loop's select");
    }
    let body = &src[src.find("    let ended = loop {").expect("the loop")..from];
    assert!(
        body.contains("custody.track_runs();")
            && method(include_str!("custody.rs"), "pub(super) fn track_runs(")
                .contains("self.run_exits.track(self.turns.runs());"),
        "every registered run is watched before the loop sleeps"
    );
    let stopped = &src[src
        .find("            Wake::Changed(open) => {")
        .expect("the change arm")..];
    let stopped = &stopped[..stopped.find("folding.refold(").expect("its fold")];
    let dirtied = method(include_str!("fold.rs"), "pub(super) fn dirtied(");
    assert!(
        stopped.contains("folding.dirtied(open)?;")
            && dirtied.contains("anyhow::bail!(\"the repo watcher stopped: {why}\")")
            && dirtied.contains(".and_then(|w| w.fault())"),
        "a closed change stream ends the monitor, saying why: {stopped}{dirtied}"
    );
    let turn = &src[from..from + src[from..].find("let owner = hold.may_act").expect("after")];
    assert!(
        turn.contains("let _turn = vigil.busy().turn();"),
        "the watchdog is armed for every turn and only then"
    );
    let deadlines: Vec<&str> = arms
        .lines()
        .filter(|line| line.contains("deadline(") || line.contains("sleep_until("))
        .map(str::trim)
        .collect();
    let named = [
        "deadline(recovery_due)",
        "deadline(turns_due)",
        "sleep_until(timer_deadline)",
        "deadline(fold_retry",
        "deadline(settles)",
        "deadline(quiesced)",
        "deadline(uc_due)",
        "deadline(waiter_retry)",
    ];
    assert_eq!(deadlines.len(), named.len(), "{deadlines:#?}");
    for deadline in named {
        assert!(
            deadlines.iter().any(|line| line.contains(deadline)),
            "{deadline} is not among {deadlines:#?}"
        );
    }
}
