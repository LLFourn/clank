//! Which zellij server a process belongs to, and how a client reaches it.
//!
//! A process can't change sessions: a pane's processes descend from the
//! server that spawned its pty, and that server was started as
//! `<exe> --server <socket path>`. So the session is read off the process
//! table, never off an inherited `ZELLIJ_SESSION_NAME`, which zellij sets
//! once at spawn and never updates.
//!
//! zellij addresses a session only by name (`--session <name>`, checked
//! against the names its own client lists), and an ID-era zellij lists a
//! socket under the name its session index gives that socket's ID. So each
//! call looks the name up from that index immediately before it runs.
//!
//! Accepted limitation: a call is bound to a name, not to a socket. If the
//! session is renamed and another one takes its old name between the lookup
//! and the call, the call reaches the other session. zellij offers no way to
//! address a socket; the window is two subprocess starts wide.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::proc_identity::ProcToken;
use crate::proc_run::{Group, Out, Spec};

/// One live zellij server: the full socket path it listens on, the
/// executable it runs, and the process it is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Endpoint {
    socket: PathBuf,
    exe: PathBuf,
    pid: i32,
    birth: ProcToken,
}

/// The server a process belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    Found(Endpoint),
    /// The whole ancestry was read and holds no zellij server.
    NotInZellij,
    /// The ancestry could not be read: denied, raced or malformed. Never
    /// evidence of being outside zellij.
    Unknown(String),
}

/// One endpoint as a scan or lookup saw it: the server, and the name it
/// had then. Equal by server alone, so a rename since is the same session;
/// the name is for messages, never for addressing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Session {
    endpoint: Endpoint,
    shown: String,
}

impl Session {
    pub(crate) fn new(endpoint: Endpoint, shown: String) -> Self {
        Self { endpoint, shown }
    }

    /// A session whose socket is named `name` in a pre-index layout, so
    /// it reads as `name` without any index.
    #[cfg(test)]
    pub(crate) fn fixture(name: &str) -> Self {
        Self {
            endpoint: Endpoint::fixture_here(format!("/tmp/zellij-test/0.0.0/{name}")),
            shown: name.to_string(),
        }
    }

    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The name it had when it was seen.
    pub(crate) fn name(&self) -> &str {
        &self.shown
    }
}

#[cfg(test)]
impl From<&str> for Endpoint {
    fn from(name: &str) -> Self {
        Session::fixture(name).endpoint
    }
}

#[cfg(test)]
impl From<&str> for Session {
    fn from(name: &str) -> Self {
        Self::fixture(name)
    }
}

#[cfg(test)]
impl Session {
    pub(crate) fn as_str(&self) -> &str {
        &self.shown
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint
    }
}

impl Eq for Session {}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.shown)
    }
}

/// How a client invocation reaches one endpoint: its server's own
/// executable, pointed at the socket root that makes the client's socket
/// directory the endpoint's, under the name the index gives it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client {
    pub(crate) program: PathBuf,
    pub(crate) socket_root: PathBuf,
    pub(crate) session: String,
}

/// What zellij's client environment variable for the socket root is.
const SOCKET_DIR_ENV: &str = "ZELLIJ_SOCKET_DIR";

/// The directory an ID-era zellij keeps its sockets under, beneath the
/// socket root; earlier releases used their version number instead.
const CONTRACT_DIR_PREFIX: &str = "contract_version_";

impl Endpoint {
    #[cfg(test)]
    pub(crate) fn fixture(socket: impl Into<PathBuf>, exe: impl Into<PathBuf>, pid: i32) -> Self {
        Self {
            socket: socket.into(),
            exe: exe.into(),
            pid,
            birth: ProcToken::MacStart { sec: 0, usec: 0 },
        }
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// An endpoint whose server is this test process, run from this
    /// test binary, so its executable is provably the one it started from.
    #[cfg(test)]
    pub(crate) fn fixture_here(socket: impl Into<PathBuf>) -> Self {
        let pid = std::process::id() as i32;
        Self {
            socket: socket.into(),
            exe: std::env::current_exe().expect("the test binary's path"),
            pid,
            birth: crate::proc_identity::token_for(pid).expect("this process's birth"),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_exe(self, exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            ..self
        }
    }

    #[cfg(test)]
    pub(crate) fn pid(&self) -> i32 {
        self.pid
    }

    /// The socket's filename: a session ID, or the name of a session from
    /// before IDs.
    pub(crate) fn id(&self) -> Option<&str> {
        self.socket.file_name()?.to_str()
    }

    fn socket_dir_name(&self) -> Option<&str> {
        self.socket.parent()?.file_name()?.to_str()
    }

    /// The `ZELLIJ_SOCKET_DIR` under which the server's own client looks in
    /// exactly the socket's directory: a client's socket directory is that
    /// root joined with its contract (or version) directory.
    fn socket_root(&self) -> Result<PathBuf, String> {
        let refuse = |why: &str| {
            Err(format!(
                "zellij server pid {} at `{}`: {why}, so no client can be pointed at its \
                 socket directory",
                self.pid,
                self.socket.display()
            ))
        };
        if !self.socket.is_absolute() {
            return refuse("the socket path is relative");
        }
        if !self.exe.is_absolute() {
            return refuse("its executable path is relative");
        }
        if self.id().is_none() || self.socket_dir_name().is_none() {
            return refuse("the socket path has no session or contract directory");
        }
        match self.socket.parent().and_then(Path::parent) {
            Some(root) if root.parent().is_some() => Ok(root.to_path_buf()),
            _ => refuse("the socket directory has no root above it"),
        }
    }

    /// The client that reaches this endpoint now, its name read from
    /// `index` at this moment. Never cache the result across calls.
    ///
    /// The client is the server's own executable, and only while
    /// `binaries` proves the file at that path is still the one the server
    /// was started from: a client looks for sockets under its OWN contract
    /// directory, and an upgrade that replaced the file under a live older
    /// server (`/root/0.44.0/clank-life` beside an installed 0.46) would
    /// look in `/root/contract_version_1` and miss it, or find another.
    pub(crate) fn client_with(
        &self,
        index: &impl NameIndex,
        binaries: &impl Binaries,
    ) -> Result<Client, String> {
        let socket_root = self.socket_root()?;
        binaries.started_from(self).map_err(|why| {
            format!(
                "zellij server pid {} at `{}`: {why}, so no client is known to use its \
                 socket directory",
                self.pid,
                self.socket.display()
            )
        })?;
        Ok(Client {
            program: self.exe.clone(),
            socket_root,
            session: self.current_name(index)?,
        })
    }

    /// [`Self::client_with`] zellij's own session index and the OS's own
    /// view of the server's executable.
    pub(crate) fn client(&self) -> Result<Client, String> {
        self.client_with(&ZellijIndex, &NativeBinaries)
    }

    /// The name zellij's CLI accepts for this endpoint now.
    pub(crate) fn current_name(&self, index: &impl NameIndex) -> Result<String, String> {
        let (Some(id), Some(dir)) = (self.id(), self.socket_dir_name()) else {
            return Err(format!(
                "`{}` names no session socket",
                self.socket.display()
            ));
        };
        if !dir.starts_with(CONTRACT_DIR_PREFIX) {
            return Ok(id.to_string());
        }
        Ok(index.name_of(dir, id)?.unwrap_or_else(|| id.to_string()))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.socket.display())
    }
}

impl Client {
    /// The process spec for `zellij --session <name> <args…>` against this
    /// endpoint, run as `program`.
    pub(crate) fn spec<I, S>(&self, program: impl AsRef<OsStr>, args: I) -> Spec
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Spec::new(program)
            .env(SOCKET_DIR_ENV, &self.socket_root)
            .env_remove("ZELLIJ_SESSION_NAME")
            .args(["--session", self.session.as_str()])
            .args(args)
    }

    /// Whether zellij's answer was that it knows no session by this
    /// client's name, which is what a rename between lookup and call gives.
    pub(crate) fn missed(&self, stderr: &str) -> bool {
        let refusal = format!("Session '{}' not found", self.session);
        stderr.lines().any(|l| l.trim_start().starts_with(&refusal))
    }
}

/// Run `call` against the client `lookup` gives, and once more against a
/// fresh lookup when zellij answered that it knows no such name. The second
/// answer is final.
pub(crate) fn dispatch<O>(
    mut lookup: impl FnMut() -> Result<Client, String>,
    mut call: impl FnMut(&Client) -> O,
    stderr: impl Fn(&O) -> Option<&str>,
) -> Result<O, String> {
    let client = lookup()?;
    let out = call(&client);
    if stderr(&out).is_some_and(|e| client.missed(e)) {
        return Ok(call(&lookup()?));
    }
    Ok(out)
}

/// Whether the file at an endpoint's executable path is the one its server
/// was started from.
pub(crate) trait Binaries {
    fn started_from(&self, ep: &Endpoint) -> Result<(), String>;
}

/// The OS's own answer, by device and inode: a positive proof of identity
/// on both platforms, never a file's age. Linux names the running file
/// (`/proc/<pid>/exe`). macOS has no such handle without `unsafe`, so
/// `lsof` names the running image instead; an `lsof` that is missing,
/// fails, or answers in a shape not read here refuses.
pub(crate) struct NativeBinaries;

impl Binaries for NativeBinaries {
    fn started_from(&self, ep: &Endpoint) -> Result<(), String> {
        if !ep.birth.still_holds(ep.pid) {
            return Err("the server process is gone or was replaced".to_string());
        }
        match &ep.birth {
            ProcToken::MacStart { .. } => matches_image(&ep.exe, lsof_text(ep.pid)),
            ProcToken::LinuxStart { .. } => {
                same_file(Path::new(&format!("/proc/{}/exe", ep.pid)), &ep.exe)
            }
            ProcToken::Unrecognised => Err("its start time is unreadable here".to_string()),
        }
    }
}

const LSOF_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
const LSOF_CAP: usize = 1 << 20;

/// macOS ships `lsof` in `/usr/sbin`, which a launchd or monitor PATH
/// can lack; a bare name there would refuse every client.
const LSOF: &str = "/usr/sbin/lsof";

/// `lsof`'s field listing of `pid`'s mapped text files: device, inode and
/// name per file, the executable image first.
fn lsof_text(pid: i32) -> Result<String, String> {
    let pid = pid.to_string();
    let out = crate::proc_run::run_blocking(
        Spec::new(LSOF)
            .args(["-a", "-p", pid.as_str(), "-d", "txt", "-FDin"])
            .stdout(Out::Capture { cap: LSOF_CAP })
            .group(Group::New),
        LSOF_BUDGET,
    )
    .map_err(|e| format!("`lsof` could not name its running image: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`lsof` could not name its running image (exit {})",
            out.status
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "`lsof` answered in non-UTF-8".to_string())
}

/// `(device, inode)` of the first `txt` file in `lsof -F` output, which is
/// the process's executable image.
pub(crate) fn running_image(lsof: &str) -> Result<(u64, u64), String> {
    let garbled = || format!("`lsof` named no running image: {lsof:?}");
    let mut lines = lsof.lines().skip_while(|l| *l != "ftxt").skip(1);
    let (mut dev, mut ino) = (None, None);
    for line in lines
        .by_ref()
        .take_while(|l| !l.starts_with('f') && !l.starts_with('p'))
    {
        if let Some(d) = line.strip_prefix('D') {
            dev = d
                .strip_prefix("0x")
                .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        } else if let Some(i) = line.strip_prefix('i') {
            ino = i.parse().ok();
        }
    }
    dev.zip(ino).ok_or_else(garbled)
}

/// Whether `exe`, resolved to the file it names now, is the image `lsof`
/// says is running.
pub(crate) fn matches_image(exe: &Path, lsof: Result<String, String>) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let running = running_image(&lsof?)?;
    let meta = std::fs::metadata(exe)
        .map_err(|e| format!("its executable `{}` can't be read: {e}", exe.display()))?;
    if (meta.dev(), meta.ino()) != running {
        return Err(format!(
            "its executable `{}` is no longer the file it runs (an upgrade replaced it?)",
            exe.display()
        ));
    }
    Ok(())
}

/// Whether `running` and `exe` are one file.
pub(crate) fn same_file(running: &Path, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let read = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| (m.dev(), m.ino()))
            .map_err(|e| format!("`{}` can't be read: {e}", p.display()))
    };
    if read(running)? != read(exe)? {
        return Err(format!(
            "its executable `{}` is no longer the file it runs (an upgrade replaced it?)",
            exe.display()
        ));
    }
    Ok(())
}

/// One process as the resolver needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) parent: i32,
    pub(crate) argv: Vec<OsString>,
    pub(crate) birth: ProcToken,
}

/// Process inspection, behind a seam so a test fakes the table.
pub(crate) trait Processes {
    /// `pid`'s parent, argv and birth, or why they could not be read.
    fn inspect(&self, pid: i32) -> Result<Proc, Inspect>;
}

/// Why one process could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inspect {
    /// The kernel says it no longer exists.
    Gone,
    Unreadable(String),
}

/// The OS's own process table, read natively.
pub(crate) struct Native;

impl Processes for Native {
    fn inspect(&self, pid: i32) -> Result<Proc, Inspect> {
        use crate::proc_identity::inspect::{Inspection, inspect};
        let Some(birth) = crate::proc_identity::token_for(pid) else {
            return Err(Inspect::Unreadable(format!(
                "pid {pid}'s identity could not be read"
            )));
        };
        match inspect(pid, &birth) {
            Inspection::Verified(d) => {
                let argv = d
                    .argv
                    .map_err(|e| Inspect::Unreadable(format!("pid {pid}'s argv: {e:?}")))?;
                let parent = d
                    .parent
                    .map_err(|e| Inspect::Unreadable(format!("pid {pid}'s parent: {e:?}")))?;
                Ok(Proc {
                    parent,
                    argv,
                    birth: d.birth,
                })
            }
            Inspection::Ended => Err(Inspect::Gone),
            Inspection::Mismatched => Err(Inspect::Unreadable(format!(
                "pid {pid} was replaced while it was read"
            ))),
            Inspection::Unavailable(why) => Err(Inspect::Unreadable(format!("pid {pid}: {why:?}"))),
        }
    }
}

/// `(exe, socket)` when `argv` is a zellij server's: a program named
/// zellij run with `--server <socket path>`. A client (`attach`, `action`,
/// a bare `zellij`) has no `--server` and is not a server.
pub(crate) fn server_argv(argv: &[OsString]) -> Option<(PathBuf, PathBuf)> {
    let (exe, rest) = argv.split_first()?;
    let exe = PathBuf::from(exe);
    let name = exe.file_name()?.to_str()?;
    if !name.starts_with("zellij") {
        return None;
    }
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--server" {
            return args.next().map(|s| (exe.clone(), PathBuf::from(s)));
        }
        if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--server=")) {
            return Some((exe, PathBuf::from(path)));
        }
    }
    None
}

/// Generous: a pane's shell, its tool and that tool's helpers are a few
/// levels, and a cycle in a raced table must not loop forever.
const MAX_ANCESTRY: usize = 256;

/// The server `pid` runs under, from `procs`.
pub(crate) fn endpoint_of_with(pid: i32, procs: &impl Processes) -> Resolved {
    let mut at = pid;
    for _ in 0..MAX_ANCESTRY {
        let p = match procs.inspect(at) {
            Ok(p) => p,
            Err(Inspect::Gone) => {
                return Resolved::Unknown(format!("pid {at} exited while its ancestry was read"));
            }
            Err(Inspect::Unreadable(why)) => return Resolved::Unknown(why),
        };
        if let Some((exe, socket)) = server_argv(&p.argv) {
            return Resolved::Found(Endpoint {
                socket,
                exe,
                pid: at,
                birth: p.birth,
            });
        }
        if p.parent <= 1 {
            return Resolved::NotInZellij;
        }
        at = p.parent;
    }
    Resolved::Unknown(format!(
        "pid {pid}'s ancestry is deeper than {MAX_ANCESTRY} processes"
    ))
}

/// The server `pid` runs under.
pub(crate) fn endpoint_of(pid: i32) -> Resolved {
    endpoint_of_with(pid, &Native)
}

/// This process's server. `Found` is kept for the process's life, since a
/// process can't change sessions; `Unknown` is asked again next time.
pub(crate) fn own_endpoint() -> Resolved {
    static FOUND: std::sync::OnceLock<Endpoint> = std::sync::OnceLock::new();
    if let Some(found) = FOUND.get() {
        return Resolved::Found(found.clone());
    }
    let resolved = endpoint_of(std::process::id() as i32);
    if let Resolved::Found(ep) = &resolved {
        let _ = FOUND.set(ep.clone());
    }
    resolved
}

/// Every live server of this user, from `ps` output of `pid uid comm`
/// lines: each zellij program's argv is read natively, and one that can't
/// be is reported rather than dropped. `Err` when the table itself could
/// not be read.
pub(crate) fn servers_in(
    table: Option<&str>,
    uid: u32,
    procs: &impl Processes,
) -> Result<Vec<Result<Endpoint, String>>, String> {
    let table = table.ok_or_else(|| "the process table could not be read".to_string())?;
    let mut found = Vec::new();
    for line in table.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(owner)) = (fields.next(), fields.next()) else {
            return Err(format!("unreadable process-table line `{line}`"));
        };
        let comm = fields.collect::<Vec<_>>().join(" ");
        let (Ok(pid), Ok(owner)) = (pid.parse::<i32>(), owner.parse::<u32>()) else {
            return Err(format!("unreadable process-table line `{line}`"));
        };
        let program = Path::new(&comm)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("");
        if owner != uid || !program.starts_with("zellij") {
            continue;
        }
        match procs.inspect(pid) {
            Ok(p) => {
                if let Some((exe, socket)) = server_argv(&p.argv) {
                    found.push(Ok(Endpoint {
                        socket,
                        exe,
                        pid,
                        birth: p.birth,
                    }));
                }
            }
            Err(Inspect::Gone) => {}
            Err(Inspect::Unreadable(why)) => {
                found.push(Err(format!("zellij process {pid}: {why}")))
            }
        }
    }
    Ok(found)
}

const PS_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
const PS_CAP: usize = 16 << 20;

/// This user's live zellij servers, by process: one per socket, whatever
/// its name or root.
pub(crate) fn live_servers() -> Result<Vec<Result<Endpoint, String>>, String> {
    let table = crate::proc_run::run_blocking(
        Spec::new("ps")
            .args(["-axo", "pid=,uid=,comm="])
            .stdout(Out::Capture { cap: PS_CAP })
            .group(Group::New),
        PS_BUDGET,
    )
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    servers_in(
        table.as_deref(),
        rustix::process::getuid().as_raw(),
        &Native,
    )
}

/// zellij's map from a socket's ID to its session's name.
pub(crate) trait NameIndex {
    /// The name of session `id` whose socket is under contract directory
    /// `contract`; `None` when the index has no row for it.
    fn name_of(&self, contract: &str, id: &str) -> Result<Option<String>, String>;
}

/// zellij's own `session_index.sqlite`, read-only.
pub(crate) struct ZellijIndex;

impl NameIndex for ZellijIndex {
    fn name_of(&self, contract: &str, id: &str) -> Result<Option<String>, String> {
        #[cfg(test)]
        if let Some(path) = tests::INDEX_STAND_IN.with(|s| s.borrow().clone()) {
            return read_name(&path, id);
        }
        let path = index_path(
            contract,
            std::env::var_os("HOME").as_deref(),
            std::env::var_os("XDG_CACHE_HOME").as_deref(),
        )?;
        read_name(&path, id)
    }
}

/// Where a zellij client run with this environment keeps its session
/// index: its platform cache dir (`directories::ProjectDirs` for
/// `org.Zellij Contributors.Zellij`) joined with its contract directory
/// (`default_index_path`, zellij-utils/src/session_index.rs).
pub(crate) fn index_path(
    contract: &str,
    home: Option<&OsStr>,
    xdg_cache: Option<&OsStr>,
) -> Result<PathBuf, String> {
    let home = home
        .filter(|h| Path::new(h).is_absolute())
        .map(PathBuf::from);
    let cache = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Caches/org.Zellij-Contributors.Zellij"))
    } else {
        xdg_cache
            .filter(|x| Path::new(x).is_absolute())
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))
            .map(|c| c.join("zellij"))
    };
    let cache = cache.ok_or_else(|| {
        "zellij's cache directory is unknown ($HOME is unset), so its session index can't \
         be read"
            .to_string()
    })?;
    Ok(cache.join(contract).join("session_index.sqlite"))
}

/// zellij's own rule for a socket's name (`query_session_name`,
/// zellij-utils/src/session_index.rs): the row whose ID, or failing that
/// whose name, is the socket's filename.
const NAME_FOR_ID: &str = "SELECT name FROM sessions WHERE COALESCE(id, name) = ?1";

/// Readers wait this long on a writer's lock before reporting it.
const INDEX_BUSY: std::time::Duration = std::time::Duration::from_millis(250);

/// The name the index at `path` gives `id`. A locked, unreadable or
/// unexpectedly shaped index is an error, never "no row". An index that
/// doesn't exist is "no row": zellij without a usable index names sockets
/// by the session's name and lists them by filename, as released 0.46
/// builds do.
pub(crate) fn read_name(path: &Path, id: &str) -> Result<Option<String>, String> {
    use rusqlite::{OpenFlags, OptionalExtension};
    let unknown = |what: &str, e: &dyn std::fmt::Display| {
        format!("zellij's session index `{}` {what}: {e}", path.display())
    };
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unknown("could not be read", &e)),
        Ok(meta) if !meta.is_file() => {
            return Err(unknown("is not a file", &"unexpected file type"));
        }
        Ok(_) => {}
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| unknown("could not be opened", &e))?;
    conn.busy_timeout(INDEX_BUSY)
        .map_err(|e| unknown("could not be read", &e))?;
    conn.query_row(NAME_FOR_ID, [id], |row| row.get::<_, String>(0))
        .optional()
        .map_err(|e| unknown("could not be read", &e))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// The index this thread's name lookups read instead of zellij's.
        pub(crate) static INDEX_STAND_IN: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// zellij 87276681's `CREATE TABLE`, verbatim
    /// (zellij-utils/src/session_index.rs `SessionIndex::open`).
    pub(crate) const ZELLIJ_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
                name TEXT PRIMARY KEY,
                folder TEXT,
                repo TEXT,
                worktree TEXT,
                branch TEXT,
                facts TEXT NOT NULL DEFAULT '{}',
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL,
                id TEXT
            );";

    /// An index file at `dir` with zellij's schema and `rows` of
    /// `(name, id)`.
    pub(crate) fn index_with(dir: &Path, rows: &[(&str, Option<&str>)]) -> PathBuf {
        let path = dir.join("session_index.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(ZELLIJ_SCHEMA).unwrap();
        for (name, id) in rows {
            conn.execute(
                "INSERT INTO sessions (name, created_at, last_used_at, id) VALUES (?1, 1, 1, ?2)",
                rusqlite::params![name, id],
            )
            .unwrap();
        }
        path
    }

    struct Table(std::collections::BTreeMap<i32, Result<Proc, Inspect>>);

    impl Table {
        fn new(rows: &[(i32, i32, &[&str])]) -> Self {
            Self(
                rows.iter()
                    .map(|(pid, parent, argv)| {
                        (
                            *pid,
                            Ok(Proc {
                                parent: *parent,
                                argv: argv.iter().map(OsString::from).collect(),
                                birth: ProcToken::MacStart {
                                    sec: *pid as u64,
                                    usec: 0,
                                },
                            }),
                        )
                    })
                    .collect(),
            )
        }
        fn failing(mut self, pid: i32, why: Inspect) -> Self {
            self.0.insert(pid, Err(why));
            self
        }
    }

    impl Processes for Table {
        fn inspect(&self, pid: i32) -> Result<Proc, Inspect> {
            self.0
                .get(&pid)
                .cloned()
                .unwrap_or(Err(Inspect::Unreadable(format!("no pid {pid}"))))
        }
    }

    const DIR: &str = "/var/folders/tj/T/zellij-501/contract_version_1";
    const EXE: &str = "/Users/u/.cargo/bin/zellij";

    fn found(r: Resolved) -> Endpoint {
        match r {
            Resolved::Found(ep) => ep,
            other => panic!("not found: {other:?}"),
        }
    }

    #[test]
    fn a_process_two_parents_below_a_server_resolves_to_its_full_socket_path() {
        let socket = format!("{DIR}/cd24109dee91");
        let table = Table::new(&[
            (300, 200, &["claude", "--resume", "x"]),
            (200, 100, &["-zsh"]),
            (100, 1, &[EXE, "--server", &socket]),
        ]);
        let ep = found(endpoint_of_with(300, &table));
        assert_eq!(ep.socket(), Path::new(&socket));
        assert_eq!(ep.pid(), 100);
        assert_eq!(ep.id(), Some("cd24109dee91"));
    }

    #[test]
    fn a_legacy_named_socket_resolves_to_its_full_path() {
        let socket = format!("{DIR}/clank-life");
        let table = Table::new(&[(7, 6, &["sh"]), (6, 1, &[EXE, "--server", &socket])]);
        assert_eq!(
            found(endpoint_of_with(7, &table)).socket(),
            Path::new(&socket)
        );
    }

    #[test]
    fn the_equals_form_of_the_server_flag_is_a_server_too() {
        let table = Table::new(&[(6, 1, &[EXE, "--server=/r/contract_version_1/abc"])]);
        assert_eq!(
            found(endpoint_of_with(6, &table)).socket(),
            Path::new("/r/contract_version_1/abc")
        );
    }

    #[test]
    fn a_complete_ancestry_without_a_server_is_not_in_zellij() {
        let table = Table::new(&[(9, 8, &["vim"]), (8, 1, &["-bash"])]);
        assert_eq!(endpoint_of_with(9, &table), Resolved::NotInZellij);
    }

    #[test]
    fn denied_or_raced_inspection_is_unknown_never_not_in_zellij() {
        for why in [Inspect::Unreadable("denied".into()), Inspect::Gone] {
            let table = Table::new(&[(9, 8, &["vim"])]).failing(8, why.clone());
            assert!(
                matches!(endpoint_of_with(9, &table), Resolved::Unknown(_)),
                "{why:?}"
            );
        }
    }

    #[test]
    fn a_zellij_client_is_not_taken_for_its_server() {
        let socket = format!("{DIR}/cd24109dee91");
        let table = Table::new(&[
            (5, 4, &["sh"]),
            (4, 3, &[EXE, "attach", "right"]),
            (
                3,
                2,
                &["/usr/local/bin/zellij", "--session", "right", "action", "x"],
            ),
            (2, 1, &[EXE, "--server", &socket]),
        ]);
        assert_eq!(found(endpoint_of_with(5, &table)).pid(), 2);
        assert_eq!(
            server_argv(&[
                OsString::from("opencode"),
                "--server".into(),
                "http://h:1".into()
            ]),
            None,
            "another program's --server is not zellij's"
        );
    }

    /// Version pin: the installed zellij (87276681,
    /// zellij-client/src/server_launch.rs `ServerLaunch::args`) starts its
    /// server as `<current_exe> --server <socket path>`, and that argv is the
    /// only evidence this module reads. If a zellij stops naming its socket
    /// there, this shape is what to recheck; the probe is
    /// `ps -axo pid=,command= | grep -- ' --server '`.
    #[test]
    fn the_server_argv_shape_zellij_spawns_is_the_one_read() {
        let recorded = [
            "/Users/llfourn/.cargo/bin/zellij",
            "--server",
            "/var/folders/tj/7ykp79tn0_753sw6tghwb5_m0000gn/T/zellij-501/contract_version_1/cd24109dee91",
        ]
        .map(OsString::from);
        let (exe, socket) = server_argv(&recorded).expect("the recorded server argv");
        assert_eq!(exe, Path::new(recorded[0].to_str().unwrap()));
        assert_eq!(socket, Path::new(recorded[2].to_str().unwrap()));
    }

    struct Names(Result<Option<String>, String>);

    struct Unchanged;

    impl Binaries for Unchanged {
        fn started_from(&self, _: &Endpoint) -> Result<(), String> {
            Ok(())
        }
    }

    impl Endpoint {
        fn client_with_ok(&self, index: &impl NameIndex) -> Result<Client, String> {
            self.client_with(index, &Unchanged)
        }
    }

    impl NameIndex for Names {
        fn name_of(&self, _: &str, _: &str) -> Result<Option<String>, String> {
            self.0.clone()
        }
    }

    #[test]
    fn the_client_runs_the_servers_exe_at_the_socket_root_under_the_indexed_name() {
        let ep = Endpoint::fixture(format!("{DIR}/cd24109dee91"), EXE, 58935);
        let client = ep.client_with_ok(&Names(Ok(Some("right".into())))).unwrap();
        assert_eq!(
            client,
            Client {
                program: PathBuf::from(EXE),
                socket_root: PathBuf::from("/var/folders/tj/T/zellij-501"),
                session: "right".into(),
            }
        );
        let spec = client.spec(&client.program, ["action", "list-tabs"]);
        let args: Vec<&OsStr> = spec.get_args().collect();
        assert_eq!(args, ["--session", "right", "action", "list-tabs"]);
        assert_eq!(spec.get_program(), OsStr::new(EXE));
    }

    #[test]
    fn an_id_without_a_row_is_its_own_name() {
        let ep = Endpoint::fixture(format!("{DIR}/cd24109dee91"), EXE, 1);
        assert_eq!(
            ep.client_with_ok(&Names(Ok(None))).unwrap().session,
            "cd24109dee91"
        );
    }

    #[test]
    fn an_unreadable_index_is_unknown_not_no_row() {
        let ep = Endpoint::fixture(format!("{DIR}/cd24109dee91"), EXE, 1);
        assert!(ep.client_with_ok(&Names(Err("locked".into()))).is_err());
    }

    #[test]
    fn a_pre_index_socket_is_named_by_its_filename_without_any_index() {
        let ep = Endpoint::fixture("/tmp/zellij-501/0.44.3/clank-clank", EXE, 1);
        let client = ep.client_with_ok(&Names(Err("no index".into()))).unwrap();
        assert_eq!(client.session, "clank-clank");
        assert_eq!(client.socket_root, Path::new("/tmp/zellij-501"));
    }

    #[test]
    fn two_roots_with_the_same_legacy_name_are_told_apart() {
        let a = Endpoint::fixture("/a/zellij-501/contract_version_1/clank-life", EXE, 10);
        let b = Endpoint::fixture("/b/zellij-501/contract_version_1/clank-life", EXE, 11);
        assert_ne!(a, b);
        let ca = a.client_with_ok(&Names(Ok(None))).unwrap();
        let cb = b.client_with_ok(&Names(Ok(None))).unwrap();
        assert_eq!(ca.session, cb.session);
        assert_ne!(ca.socket_root, cb.socket_root);
    }

    /// The caller's own socket root and session name never reach the
    /// client: the spec sets the endpoint's root and drops the inherited
    /// name, whatever this test process inherited.
    #[test]
    fn a_stale_caller_environment_does_not_reach_the_client() {
        let ep = Endpoint::fixture(format!("{DIR}/cd24109dee91"), EXE, 1);
        let client = ep.client_with_ok(&Names(Ok(Some("right".into())))).unwrap();
        let env = client
            .spec(&client.program, ["action", "x"])
            .get_environment();
        assert_eq!(
            env.get(OsStr::new("ZELLIJ_SOCKET_DIR"))
                .map(OsString::as_os_str),
            Some(OsStr::new("/var/folders/tj/T/zellij-501"))
        );
        assert!(!env.contains_key(OsStr::new("ZELLIJ_SESSION_NAME")));
    }

    #[test]
    fn a_client_that_cannot_be_pointed_at_the_directory_is_refused() {
        for (socket, exe) in [
            ("relative/contract_version_1/abc", EXE),
            ("/contract_version_1", EXE),
            ("/abc", EXE),
            ("/r/contract_version_1/abc", "zellij"),
        ] {
            let ep = Endpoint::fixture(socket, exe, 1);
            assert!(
                ep.client_with_ok(&Names(Ok(None))).is_err(),
                "{socket} with {exe} must refuse"
            );
        }
    }

    /// An upgrade replaces a zellij binary under a live older server: the
    /// server keeps running from the file it started with, and the path now
    /// names a newer one. No client is built from it.
    #[test]
    fn a_binary_replaced_under_a_live_server_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let upgraded = dir.path().join("zellij");
        std::fs::write(&upgraded, "a newer zellij").unwrap();
        let ep = Endpoint::fixture_here("/root/0.44.0/clank-life").with_exe(&upgraded);
        let refused = ep.client().unwrap_err();
        assert!(refused.contains("replaced"), "{refused}");

        let original = Endpoint::fixture_here("/root/0.44.0/clank-life");
        assert_eq!(original.client().unwrap().session, "clank-life");
    }

    /// `lsof -F` naming the file at `path` as a process's first text file.
    fn lsof_for(path: &Path) -> String {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).unwrap();
        format!(
            "p1\nftxt\nD{:#x}\ni{}\nn{}\nftxt\nD0x1\ni7\nn/usr/lib/dyld\n",
            m.dev(),
            m.ino(),
            path.display()
        )
    }

    /// The image a fake `lsof` names, so a test chooses what is running.
    struct Running(Result<String, String>);

    impl Binaries for Running {
        fn started_from(&self, ep: &Endpoint) -> Result<(), String> {
            matches_image(&ep.exe, self.0.clone())
        }
    }

    /// Codex's regression: after the server started, its executable path
    /// is replaced by a symlink to an already-existing, older file. Older
    /// is not the same: no client is built.
    #[test]
    fn a_path_relinked_to_an_older_file_is_refused_before_any_call() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("zellij-0.44");
        std::fs::write(&started, "the running zellij").unwrap();
        let older = dir.path().join("older");
        std::fs::write(&older, "an older file").unwrap();
        let path = dir.path().join("zellij");
        std::os::unix::fs::symlink(&older, &path).unwrap();
        let ep = Endpoint::fixture("/root/0.44.0/clank-life", &path, 1);
        let running = Running(Ok(lsof_for(&started)));
        let refused = ep.client_with(&Names(Ok(None)), &running).unwrap_err();
        assert!(refused.contains("no longer the file it runs"), "{refused}");
    }

    #[test]
    fn the_running_file_or_a_hard_link_to_it_is_accepted_and_a_copy_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let running = dir.path().join("zellij");
        std::fs::write(&running, "x").unwrap();
        let link = dir.path().join("linked");
        std::fs::hard_link(&running, &link).unwrap();
        let copy = dir.path().join("copy");
        std::fs::write(&copy, "x").unwrap();
        let lsof = || Ok(lsof_for(&running));
        assert!(matches_image(&running, lsof()).is_ok());
        assert!(matches_image(&link, lsof()).is_ok());
        assert!(
            matches_image(&copy, lsof()).is_err(),
            "same bytes, another file"
        );
    }

    #[test]
    fn an_lsof_that_is_missing_or_garbled_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("zellij");
        std::fs::write(&exe, "x").unwrap();
        assert!(matches_image(&exe, Err("`lsof` could not run".into())).is_err());
        for garbled in [
            "",
            "p1\n",
            "p1\nfcwd\nD0x1\ni2\n",
            "p1\nftxt\nn/x\n",
            "p1\nftxt\nD16777235\ni2\n",
            "p1\nftxt\nD0x1\niNaN\n",
        ] {
            assert!(
                matches_image(&exe, Ok(garbled.to_string())).is_err(),
                "{garbled:?}"
            );
        }
        assert_eq!(
            running_image("p58935\nftxt\nD0x1000013\ni325442815\nn/z\nftxt\nD0x1\ni9\n"),
            Ok((0x1000013, 325442815)),
            "the first text file is the image"
        );
    }

    #[test]
    fn only_the_running_file_itself_is_the_running_file() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, link) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("l"),
        );
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        std::fs::hard_link(&a, &link).unwrap();
        assert!(same_file(&a, &link).is_ok());
        assert!(
            same_file(&a, &b).is_err(),
            "identical bytes are not one file"
        );
    }

    fn client(name: &str) -> Client {
        Client {
            program: EXE.into(),
            socket_root: "/r".into(),
            session: name.into(),
        }
    }

    #[test]
    fn a_rename_between_lookup_and_call_is_retried_once_with_the_new_name() {
        let names = std::cell::RefCell::new(vec!["new", "old"]);
        let mut tried = Vec::new();
        let out = dispatch(
            || Ok(client(names.borrow_mut().pop().unwrap())),
            |c| {
                tried.push(c.session.clone());
                if c.session == "old" {
                    "Session 'old' not found. The following sessions are active:\nnew".to_string()
                } else {
                    String::new()
                }
            },
            |o| Some(o.as_str()),
        )
        .unwrap();
        assert_eq!(tried, ["old", "new"]);
        assert_eq!(out, "");
    }

    #[test]
    fn a_second_miss_is_reported_not_retried_again() {
        let mut lookups = 0;
        let mut calls = 0;
        let out = dispatch(
            || {
                lookups += 1;
                Ok(client("gone"))
            },
            |_| {
                calls += 1;
                "Session 'gone' not found.".to_string()
            },
            |o| Some(o.as_str()),
        )
        .unwrap();
        assert_eq!((lookups, calls), (2, 2));
        assert!(out.contains("not found"));
    }

    #[test]
    fn a_miss_for_another_name_or_another_failure_is_not_retried() {
        for stderr in ["Session 'other' not found.", "permission denied"] {
            let mut calls = 0;
            dispatch(
                || Ok(client("mine")),
                |_| {
                    calls += 1;
                    stderr.to_string()
                },
                |o| Some(o.as_str()),
            )
            .unwrap();
            assert_eq!(calls, 1, "{stderr}");
        }
    }

    #[test]
    fn an_unknown_name_never_reaches_the_call() {
        let mut calls = 0;
        let out = dispatch(|| Err("index locked".to_string()), |_| calls += 1, |_| None);
        assert!(out.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn the_index_gives_an_ids_current_name_and_no_row_for_a_stranger() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_with(
            dir.path(),
            &[
                ("right", Some("cd24109dee91")),
                ("clank-life-renamed", Some("clank-life")),
            ],
        );
        assert_eq!(
            read_name(&path, "cd24109dee91").unwrap().as_deref(),
            Some("right")
        );
        assert_eq!(
            read_name(&path, "clank-life").unwrap().as_deref(),
            Some("clank-life-renamed")
        );
        assert_eq!(read_name(&path, "aaaaaaaaaaaa").unwrap(), None);
    }

    /// A row from before the `id` column: its name is its ID, as zellij's
    /// `COALESCE(id, name)` reads it.
    #[test]
    fn a_row_without_an_id_is_found_by_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_with(dir.path(), &[("clank-fs", None)]);
        assert_eq!(
            read_name(&path, "clank-fs").unwrap().as_deref(),
            Some("clank-fs")
        );
    }

    #[test]
    fn an_absent_index_names_the_socket_by_its_filename() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_name(&dir.path().join("session_index.sqlite"), "clank-life"),
            Ok(None)
        );
    }

    #[test]
    fn an_index_path_that_is_not_a_file_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.sqlite");
        std::fs::create_dir(&path).unwrap();
        assert!(read_name(&path, "abc").is_err());
    }

    #[test]
    fn a_reshaped_index_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_index.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE sessions (label TEXT PRIMARY KEY, socket TEXT);")
            .unwrap();
        drop(conn);
        assert!(read_name(&path, "abc").is_err());
    }

    #[test]
    fn a_locked_index_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_with(dir.path(), &[("right", Some("abc"))]);
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let read = read_name(&path, "abc");
        writer.execute_batch("ROLLBACK;").unwrap();
        assert!(read.is_err(), "{read:?}");
    }

    #[test]
    fn the_index_lives_under_the_clients_cache_dir_and_contract() {
        let path = index_path(
            "contract_version_1",
            Some(OsStr::new("/Users/u")),
            Some(OsStr::new("/x/cache")),
        )
        .unwrap();
        let want = if cfg!(target_os = "macos") {
            "/Users/u/Library/Caches/org.Zellij-Contributors.Zellij/contract_version_1/session_index.sqlite"
        } else {
            "/x/cache/zellij/contract_version_1/session_index.sqlite"
        };
        assert_eq!(path, Path::new(want));
        assert!(index_path("contract_version_1", None, None).is_err());
    }

    #[test]
    fn a_server_table_lists_this_users_servers_and_reports_the_unreadable() {
        let table = Table::new(&[
            (10, 1, &[EXE, "--server", "/r/contract_version_1/a"]),
            (11, 1, &[EXE, "attach", "a"]),
            (13, 1, &[EXE, "--server", "/r/contract_version_1/c"]),
        ])
        .failing(12, Inspect::Unreadable("denied".into()))
        .failing(14, Inspect::Gone);
        let ps = format!(
            "10 501 {EXE}\n11 501 zellij\n12 501 zellij\n13 502 zellij\n14 501 zellij\n15 501 /bin/zsh\n"
        );
        let found = servers_in(Some(&ps), 501, &table).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found[0].as_ref().unwrap().socket(),
            Path::new("/r/contract_version_1/a")
        );
        assert!(found[1].is_err(), "an unreadable zellij is reported");
        assert!(servers_in(None, 501, &table).is_err());
    }
}
