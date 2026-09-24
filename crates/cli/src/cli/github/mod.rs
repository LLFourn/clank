//! Watching GitHub: the event feed, the CI conclusions read from a
//! PR's checks, the write-ahead inbox they land in, and the timeline
//! that merges what every agent saw.
//!
//! One directory rather than one 7500-line file, along the seams the
//! code already had (`the-github-watcher-is-a-directory`). Nothing
//! here changed shape in the move; what belongs where is the only
//! thing it decides.

/// A PR head's CI result: the reduction over every check reporting on
/// it, the reader that fetches them, and what a watch remembers.
mod ci;
/// The emission gate every wake passes, and the tick that drives the
/// feed and the checks on their own schedules.
mod coordinator;
/// A pending review's draft comments, read as state because nothing
/// delivers an edit to one.
pub(crate) mod draft;
/// What an event means and which ones are new: both classifiers, the
/// poll state machine, and the cursor a watch remembers.
mod feed;
/// The per-agent write-ahead inbox every ingested event lands in
/// before it is presented.
pub(crate) mod log;
/// The realtime relay: the forwarder, its loopback listener, and the
/// bounded inbox deliveries wait in.
mod relay;
/// The authorized session every GitHub read goes through.
pub(crate) mod session;
/// Driving one armed watch: the ingest lease, the follower tail and
/// the takeover.
pub(crate) mod supervisor;
/// Merging one repo's events across the agents that saw them.
pub(crate) mod timeline;
