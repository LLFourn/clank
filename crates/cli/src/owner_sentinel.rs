//! Death-with-owner for a wait that has no timeout of its own
//! (remove-wait-timeout).
//!
//! The spawner hands the child a pipe as stdin and holds the write
//! end. EOF on that pipe means every write end is closed, which the
//! kernel guarantees on orderly exit AND on SIGKILL alike — so it
//! reports owner death through paths where destructors never run and
//! `kill_on_drop` cannot help.
//!
//! Sampling `getppid()` was rejected for two reasons this design does
//! not have: an owner dying between spawn and the child's first
//! instruction leaves the child holding the reaper's pid and waiting
//! forever, and a check on the work loop's heartbeat cannot fire while
//! the main thread is blocked in setup (`notify`'s watch has been
//! measured at 7.5-11.3s under libtest). EOF is already pending in the
//! first case, and the watcher below owns a thread of its own in the
//! second.

use std::io::Read;

/// Block until `owner` reaches EOF — that is, until the last write end
/// closes.
///
/// Pure and synchronous so the exit policy stays with the caller and
/// the mechanism can be driven by a test over a real pipe. A read
/// error is treated as owner loss: the handle is unusable either way,
/// and staying alive on a broken sentinel is the failure this exists
/// to prevent.
pub fn block_until_owner_gone<R: Read>(mut owner: R) {
    let mut scratch = [0u8; 64];
    loop {
        match owner.read(&mut scratch) {
            Ok(0) => return,
            // Bytes are not a protocol — only the close is. Anything
            // written is ignored, but yielding first keeps a stdin
            // that is an endless readable stream (`< /dev/zero`,
            // i.e. misuse of the flag) from spinning a core. EOF
            // still returns promptly, so this costs no reap latency.
            Ok(_) => {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Watch stdin for owner death on a dedicated thread and exit the
/// process when it lands.
///
/// Exits rather than signalling the loop: the whole point is to be
/// responsive while the main thread is blocked somewhere that cannot
/// poll. Nobody is left to read our output once the owner is gone, so
/// the status is a plain success.
pub fn spawn_stdin_sentinel() {
    std::thread::spawn(|| {
        block_until_owner_gone(std::io::stdin().lock());
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn returns_when_the_write_end_closes_while_parked() {
        let (reader, writer) = std::io::pipe().unwrap();
        let watcher = std::thread::spawn(move || block_until_owner_gone(reader));
        // Still parked: the owner is alive, so this must not return.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!watcher.is_finished(), "returned while the owner was alive");
        drop(writer);
        watcher.join().expect("watcher thread panicked");
    }

    #[test]
    fn returns_immediately_when_the_owner_died_before_the_watch_began() {
        // The startup race that sank the getppid design: the owner is
        // already gone before the child looks. EOF is pending, so the
        // watcher must not park.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let watcher = std::thread::spawn(move || block_until_owner_gone(reader));
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            watcher.is_finished(),
            "parked despite the owner already being gone"
        );
        watcher.join().expect("watcher thread panicked");
    }

    #[test]
    fn traffic_on_the_pipe_is_not_owner_death() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let watcher = std::thread::spawn(move || block_until_owner_gone(reader));
        for _ in 0..3 {
            writer.write_all(b"noise").unwrap();
            writer.flush().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!watcher.is_finished(), "bytes were read as a close");
        drop(writer);
        watcher.join().expect("watcher thread panicked");
    }

    #[test]
    fn a_write_end_still_held_elsewhere_keeps_the_wait_alive() {
        // EOF is "the LAST write end closed" — a duplicated handle
        // (the shape of a spawner that keeps its own copy) must not
        // read as death when only one copy drops.
        let (reader, writer) = std::io::pipe().unwrap();
        let duplicate = writer.try_clone().unwrap();
        let watcher = std::thread::spawn(move || block_until_owner_gone(reader));
        drop(writer);
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!watcher.is_finished(), "one closed copy read as death");
        drop(duplicate);
        watcher.join().expect("watcher thread panicked");
    }
}
