use super::*;
use crate::cli::monitor::fixtures::*;

/// A fold that FAILED must not advertise progress: it arms the
/// watchdog from the first failure until a fold completes. Tested on
/// the ORDERING with the fold's outcome supplied, because the rebuild
/// is fail-soft — it returns `Ok` for a repo with no git, a broken
/// `.git` file and an empty `.git` directory — so a failing fold
/// cannot be provoked through it.
#[test]
fn a_failed_fold_does_not_advertise_progress() {
    let busy = watchdog::Busy::default();
    let failing = |busy: &watchdog::Busy| {
        matches!(
            watchdog::look(
                None,
                busy.failing_since(),
                Retiring::No,
                None,
                std::time::Instant::now() + watchdog::BUDGET,
                watchdog::BUDGET,
            ),
            watchdog::Look::StandDown(_)
        )
    };
    let failed = advance_with(0, &busy, Err(anyhow::anyhow!("the fold did not complete")));
    assert!(failed.is_none(), "a failed fold yields nothing to serve");
    assert!(failing(&busy), "and the watchdog is armed by it");
    let ok = advance_with(
        0,
        &busy,
        Ok(crate::repo_state::RepoState::empty(
            std::path::PathBuf::from("/w/proj"),
        )),
    )
    .expect("a completed fold serves");
    assert_eq!(ok.0, 1);
    assert!(!failing(&busy), "and a completed one disarms it");
}

/// With nothing changing, the loop's change producer says nothing,
/// however much time passes: there is no heartbeat to make a fold of.
#[tokio::test(start_paused = true)]
async fn no_change_is_no_wake_however_long_it_is() {
    let (_fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
    let mut changes = changes(fs_rx);
    for _ in 0..24 {
        tokio::time::advance(Duration::from_secs(3600)).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(3600), changes.recv())
            .await
            .is_err(),
        "a day of nothing woke the loop"
    );
}

/// A filesystem wake survives the owner being busy.
///
/// The old shape could consume the wake and then be cancelled
/// during its 200ms debounce — the channel item gone, the change
/// never delivered. A dropped repo change is a silent agent.
///
/// REAL time, not paused: `Beat::bridge` forwards from the std
/// channel on an OS thread, and paused time does not wait for
/// thread scheduling.
#[tokio::test]
async fn a_filesystem_wake_is_not_lost_while_the_owner_is_busy() {
    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
    let mut changes = changes(fs_rx);
    fs_tx.send(()).unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), changes.recv()).await;
    assert!(
        matches!(change, Ok(Some(()))),
        "the wake was consumed and then discarded"
    );
}

/// A burst is one refold, and changes coalesce for a busy owner
/// rather than queueing a backlog of folds, each already superseded.
#[tokio::test]
async fn a_burst_of_changes_is_one_refold() {
    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
    let mut changes = changes(fs_rx);
    for _ in 0..20 {
        fs_tx.send(()).unwrap();
    }
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), changes.recv()).await,
            Ok(Some(()))
        ),
        "the burst produced no wake"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(600), changes.recv())
            .await
            .is_err(),
        "the burst produced more than one refold"
    );
    drop(fs_tx);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), changes.recv())
            .await
            .ok(),
        Some(None),
        "a watcher gone closes the channel"
    );
}
