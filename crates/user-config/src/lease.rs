//! The one non-blocking `flock` take. Here because this is the crate
//! every lease holder in the workspace depends on.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// How long a Busy answer is retried before it is believed.
pub const PATIENCE: Duration = Duration::from_millis(250);

/// Take `file`'s exclusive lock if no one else holds it: `Ok(false)`
/// when someone does.
///
/// A spawn in flight briefly holds a copy of every descriptor, on every
/// spawn path (measured: fork, std's and our own `posix_spawn`), and the
/// copy shares the lock until it closes a few milliseconds later. A
/// Busy is therefore retried with backoff for [`PATIENCE`]: a copy lets
/// go within that, a real holder does not. Blocks the calling thread
/// that long when the lock is genuinely held.
pub fn take(file: &File) -> io::Result<bool> {
    let by = Instant::now() + PATIENCE;
    let mut pause = Duration::from_millis(1);
    loop {
        if take_now(file)? {
            return Ok(true);
        }
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(Duration::from_millis(32));
    }
}

/// One attempt, for a caller that is itself a retry loop until its own
/// deadline and must not block in between. A Busy here may be a spawn's
/// passing copy, so it is never final on its own.
pub fn take_now(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: a valid owned fd; flock has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(e),
        }
    }
}
