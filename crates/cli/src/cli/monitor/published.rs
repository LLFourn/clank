use super::runtime::{runtime_dir, runtime_path, runtime_path_in};
use super::socket::{Trust, ensure_dir, trusted_dir};
use super::wire::{DescribedBuild, Descriptor, Envelope, PROTO, admits};

/// Publish a descriptor by RENAME, never by truncating in place.
///
/// A client reads this without the lease, so a partial write would be
/// read as a corrupt descriptor — and truncating in place can leave the
/// tail of a longer predecessor behind when the new one is shorter.
/// Rename is atomic within a directory, so a reader sees one whole
/// descriptor or the previous one (codex on 06ccd66).
pub(crate) fn publish(repo: &std::path::Path, d: &Descriptor) -> std::io::Result<()> {
    let me = rustix::process::getuid().as_raw();
    publish_in(repo, &runtime_dir(me), me, d)
}

/// [`publish`] with the namespace supplied, so a test can hand it one
/// it must refuse.
///
/// Vetted, like every other mutation boundary in this namespace, and
/// for a sharper reason than the others: the descriptor is how a
/// client FINDS the socket. A directory another uid planted in
/// world-writable `/tmp` would take a descriptor naming THEIR socket,
/// and every client would connect to it. An unvetted `create_dir_all`
/// was also a way to create the namespace at the umask's mode — 0o775
/// under 002 — which `dir_is_acceptable` then refuses forever, so the
/// first publish on such a machine locked every later lease out of its
/// own directory.
pub(crate) fn publish_in(
    repo: &std::path::Path,
    dir: &std::path::Path,
    me: u32,
    d: &Descriptor,
) -> std::io::Result<()> {
    ensure_dir(dir, me).map_err(std::io::Error::other)?;
    let final_path = runtime_path_in(dir, repo, "json");
    // Named for the writer so a stray temp from a dead monitor cannot
    // be mistaken for this one's.
    let tmp = final_path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(d)?)?;
    std::fs::rename(&tmp, &final_path)
}

/// What a repo's monitor published, if anything readable.
#[cfg(test)]
pub(crate) fn read_published(repo: &std::path::Path) -> Option<Descriptor> {
    match published(repo) {
        Published::Ours(d) => Some(d),
        _ => None,
    }
}

/// What a repo's published descriptor turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Published {
    /// Ours, and its body parsed.
    Ours(Descriptor),
    /// A monitor of ANOTHER version published this. The number is
    /// trusted because it was read first; the socket only when the
    /// rest of the body happened to parse, which is why it is
    /// optional. Without one there is no way to reach the holder, and
    /// a successor must say that rather than guess a path.
    ///
    /// The pid likewise, read on its own so that a body whose other
    /// fields changed can still be asked whether its process lives.
    Other {
        proto: u32,
        pid: Option<i32>,
        socket: Option<String>,
    },
    /// Nothing there, or nothing readable even as a version.
    Nothing,
    /// The NAMESPACE is not ours, so nothing read from it is evidence
    /// of anything. Distinct from `Nothing` because an absence invites
    /// starting a monitor and this does not: the lease lives in the
    /// same namespace and cannot be taken either.
    Untrusted(String),
}

/// What the runtime namespace says about this repo's monitor.
pub(crate) fn published(repo: &std::path::Path) -> Published {
    let me = rustix::process::getuid().as_raw();
    published_in(&runtime_dir(me), repo, me)
}

/// Read the descriptor VERSION FIRST, as every frame on the socket is.
///
/// Serde parses a struct whole, so a descriptor whose SHAPE changed
/// fails before its `proto` can be read, and the reader cannot tell "a
/// monitor of another version is here" from "no monitor is here". The
/// first is a candidate to replace; the second is a reason to start
/// one. Collapsing them is how a successor would spawn against a live
/// holder for ever (codex on 258d979).
pub(crate) fn published_in(dir: &std::path::Path, repo: &std::path::Path, me: u32) -> Published {
    match trusted_dir(dir, me) {
        Trust::Ours => {}
        Trust::Absent => return Published::Nothing,
        Trust::Theirs(why) => return Published::Untrusted(why),
    }
    let Ok(raw) = std::fs::read(runtime_path_in(dir, repo, "json")) else {
        return Published::Nothing;
    };
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&raw) else {
        return Published::Nothing;
    };
    let body = serde_json::from_slice::<Descriptor>(&raw).ok();
    let pid = serde_json::from_slice::<PidEnvelope>(&raw)
        .ok()
        .map(|p| p.pid);
    if !admits(PROTO, envelope.proto) {
        return Published::Other {
            proto: envelope.proto,
            pid,
            // An older descriptor still parses here when it only ADDED
            // fields we now ignore, which is how a successor reaches
            // the holders this plan exists for.
            socket: body.map(|d| d.socket),
        };
    }
    match body {
        Some(d) => Published::Ours(d),
        // Our own number on a body we cannot read. Not a candidate and
        // not an absence: something is there and it is not what it
        // says it is.
        None => Published::Other {
            proto: envelope.proto,
            pid,
            socket: None,
        },
    }
}

#[derive(serde::Deserialize)]
struct PidEnvelope {
    pid: i32,
}

pub(crate) fn describe(repo: &std::path::Path, pid: i32, socket: &std::path::Path) -> Descriptor {
    Descriptor {
        proto: PROTO,
        pid,
        repo: repo.to_string_lossy().to_string(),
        socket: socket.to_string_lossy().to_string(),
        build: None,
    }
}

/// What one monitor process has seen of its installation: kept apart
/// from the routing descriptor, under that process's own name, so it can
/// be written at any time without touching where clients connect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Observation {
    pub(crate) pid: i32,
    pub(crate) birth: Option<crate::proc_identity::ProcToken>,
    pub(crate) running: String,
    pub(crate) installed_at: String,
    pub(crate) source: crate::build_stamp::Source,
    pub(crate) state: Observed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Observed {
    Current,
    Replaced {
        installed: String,
        /// `None` when the installed build embeds no version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
    },
    Unavailable {
        why: String,
    },
}

impl From<&crate::build_stamp::Install> for Observed {
    fn from(install: &crate::build_stamp::Install) -> Self {
        use crate::build_stamp::Install;
        match install {
            Install::Current => Observed::Current,
            Install::Replaced(build) => Observed::Replaced {
                installed: build.stamp.to_string(),
                installed_version: build.version.clone(),
            },
            Install::Unavailable(why) => Observed::Unavailable { why: why.clone() },
        }
    }
}

const OBSERVATION_PREFIX: &str = "build-";

pub(super) fn observation_path(repo: &std::path::Path, pid: i32) -> std::path::PathBuf {
    runtime_path(repo, &format!("{OBSERVATION_PREFIX}{pid}.json"))
}

/// Write `observation` as its process's record for `repo`, by rename.
pub(crate) fn note_observation(
    repo: &std::path::Path,
    observation: &Observation,
) -> std::io::Result<()> {
    let path = observation_path(repo, observation.pid);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(observation)?)?;
    std::fs::rename(tmp, path)
}

/// Remove the observation record of the monitor `repo`'s descriptor
/// names, if that monitor died: every other way a monitor ends removes
/// its own. Looked up by name, not by listing the runtime directory,
/// which every repo on the machine shares and which a test suite leaves
/// tens of thousands of entries in: a listing there cost a starting
/// monitor seconds before it could publish.
pub(super) fn forget_dead_observation(repo: &std::path::Path) {
    let Some(pid) = std::fs::read(runtime_path(repo, "json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<PidEnvelope>(&raw).ok())
        .map(|p| p.pid)
    else {
        return;
    };
    let path = observation_path(repo, pid);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let alive = serde_json::from_slice::<Observation>(&bytes)
        .ok()
        .is_some_and(|o| o.birth.is_some_and(|b| b.still_holds(o.pid)));
    if !alive {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a repo's monitor process exists, and the build it serves.
///
/// Existence only: a `Running` monitor is not thereby responsive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// The published pid is alive and, where its observation record
    /// carries a birth, still the process that wrote it.
    Running {
        pid: i32,
        /// `None` when the descriptor carries none, as a legacy or
        /// other-version one does not.
        build: Option<DescribedBuild>,
        /// What it last recorded of its installation, when a record
        /// holds for this process.
        observed: Option<Observed>,
    },
    /// The published pid is dead, or is a stranger by its recorded birth.
    Gone {
        pid: i32,
    },
    /// Something is published but no pid can be believed from it.
    Unknown(String),
    None,
}

impl Liveness {
    /// The `monitor:` line of `clank status`.
    pub(crate) fn line(&self) -> String {
        match self {
            Liveness::Running {
                pid,
                build,
                observed,
            } => describe_build(*pid, build.as_ref(), observed.as_ref()),
            Liveness::Gone { pid } => format!("gone (pid {pid} exited)"),
            Liveness::Unknown(why) => format!("unknown: {why}"),
            Liveness::None => "none".to_string(),
        }
    }
}

/// Whether `repo`'s monitor is running: the routing descriptor joined to
/// the observation record of the process it names.
pub(crate) fn liveness(repo: &std::path::Path) -> Liveness {
    match published(repo) {
        Published::Ours(d) => serving(repo, d.pid, d.build),
        Published::Other { pid: Some(pid), .. } => serving(repo, pid, None),
        Published::Other {
            proto, pid: None, ..
        } => Liveness::Unknown(format!("a proto {proto} descriptor with no readable pid")),
        // Nothing read from a namespace we do not trust is evidence, of
        // a monitor or of its absence.
        Published::Untrusted(why) => Liveness::Unknown(why),
        Published::Nothing => Liveness::None,
    }
}

pub(super) fn serving(repo: &std::path::Path, pid: i32, build: Option<DescribedBuild>) -> Liveness {
    if !crate::signal::alive(pid) {
        return Liveness::Gone { pid };
    }
    let recorded = std::fs::read(observation_path(repo, pid))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Observation>(&bytes).ok())
        .filter(|o| o.pid == pid)
        .and_then(|o| o.birth.map(|birth| (birth, o.state)));
    let observed = match recorded {
        Some((birth, state)) if birth.still_holds(pid) => Some(state),
        // A record without a birth this build can compare proves
        // nothing either way, so the live pid stands unobserved.
        None | Some((crate::proc_identity::ProcToken::Unrecognised, _)) => None,
        Some(_) => return Liveness::Gone { pid },
    };
    Liveness::Running {
        pid,
        build,
        observed,
    }
}

pub(super) fn describe_build(
    pid: i32,
    build: Option<&DescribedBuild>,
    observed: Option<&Observed>,
) -> String {
    let Some(build) = build else {
        return format!("pid {pid}, build unknown (legacy)");
    };
    let state = match observed {
        Some(Observed::Current) => "current".to_string(),
        Some(Observed::Replaced { installed, .. }) => {
            format!("replaced by {installed}, retiring")
        }
        Some(Observed::Unavailable { why }) => format!("install path unavailable: {why}"),
        None => "not yet observed".to_string(),
    };
    let fallback = if build.source == crate::build_stamp::Source::Fallback {
        " (fallback path)"
    } else {
        ""
    };
    format!(
        "pid {pid}, build {} at {}{fallback}: {state}",
        build.running, build.installed_at
    )
}

#[cfg(test)]
#[path = "published_tests.rs"]
mod tests;
