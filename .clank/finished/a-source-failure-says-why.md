# a-source-failure-says-why
# A command source's failure event carries why it stopped: the last exit, signal or spawn error, and a bounded tail of its stderr

## Why

anxiety-attack's codex, investigating a failure of life's anx voice source for life
(`assistent command-voice-7f9bc9f9@171`, epoch 1792018249), could not tell what went
wrong:
- **The failure event says only that it stopped.** `command_source::report_failure`,
  called from `wait.rs` once `FAILURES_BEFORE_SURFACING` is reached, is given only
  "`<argv>` has stopped N times in a row and is being restarted". The exit status,
  launch error and stderr are not in it.
- **The producer's stderr goes nowhere.** It is re-printed by a detached task as
  `wait: command source …` lines, but the monitor runs with stderr sent to
  `/dev/null` (`spawn_monitor`), so for a hosted source it is discarded.
- **The lifecycle forgets.** `Lifecycle::BackingOff` holds the last exit or error
  only until the next `Running` replaces it.

So a source failure can only be diagnosed by guesswork.

## What to build

### 1. Bounded streaming capture

Each run's stderr is read in fixed chunks (4 KiB reads), never with
`lines()`/`next_line()`, which can accumulate an unterminated line of any size.
- **The retained tail:** only the last `TAIL_BYTES` (4 KiB) of raw stderr bytes are
  kept, in a fixed-size buffer that drops older bytes as newer ones arrive. That
  holds however the bytes are split: a single unterminated line of megabytes,
  invalid UTF-8, control bytes, or EOF without a newline.
- **Memory is bounded** by the chunk plus the tail, whatever the producer writes.
- **Rendering**, when a run is snapshotted:
  1. the raw tail is decoded lossily (invalid UTF-8 becomes U+FFFD);
  2. its first partial line is dropped if the tail was truncated, so a fragment cut
     at the boundary is not shown;
  3. at most the last 20 lines are kept;
  4. sanitizing follows (3).

  The rendered text is then capped at 8 KiB, after escaping and redaction, with a
  marker if cut.
- **The human echo only reaches a terminal.** The existing echo of producer stderr
  to the wait's own stderr (`wait: command source …: <line>`) is kept only when that
  stderr is a terminal (`isatty(2)`), for a person watching.
  - When it is piped, as in the stop hook's `clank wait` (whose parent collects the
    child's whole stderr with `wait_with_output`) or the monitor (stderr sent to
    `/dev/null`), there is no live echo at all. The producer's stderr is still
    drained continuously by the reader, so the producer never blocks on a full
    pipe, and the bounded tail is kept for the failure event.
  - The echo, where it is enabled, comes from the same chunked reader. It assembles
    lines in a buffer of at most 1 KiB: a longer line is cut and the rest discarded
    up to the next newline, never assembled whole. Each echoed line goes through the
    same sanitizer.

### 2. Run ownership, and the capture cutoff

- **One reader per run.** Each run creates its own tail buffer, and that run's
  stderr reader writes only into it. A reader from an earlier run can never touch a
  later run's buffer. No run counter or persistent state is added.
- **How it ended.** The run's end is taken from its `ExitStatus`: the exit code, or
  the terminating signal (`ExitStatusExt::signal`), so a signalled run is never "no
  code". For a run that never started, the launch error is used.
- **The cutoff.** When the root exits, the reader gets the existing bounded root-exit
  drain to finish what is already written:
  - 100 ms, or until the drain cutoff during a monitor drain, as the stdout reader
    already gets;
  - EOF ends it sooner;
  - the reader is then stopped and the run's tail snapshotted.

  This is one absolute cutoff, shared by the stdout and stderr readers. stderr is
  not given a fresh 100 ms after stdout's window, so restart and drain timing are as
  today. It does not wait for EOF beyond that. A descendant holding stderr open cannot delay
  restart or backoff; its later writes go nowhere.
- **The snapshot.** The run's end and its snapshotted tail are the run's cause. The
  most recent failed run's cause is what the failure event reports.

### 3. One sanitizer for the whole diagnostic

Everything the feature puts in front of the agent or a person goes through one
sanitizer before it is stored or shown: the command line (argv), the launch error,
the stderr tail, and the echoed stderr lines. It:
- escapes terminal control characters (everything below 0x20 except the newlines
  between tail lines, and 0x7f) as `\xNN`;
- masks the value after `token=`, `password=`, `secret=` and `api_key=`, matched
  case-insensitively, including in `--flag=value` form;
- masks `Authorization:` header values, `Bearer <value>`, and URL userinfo
  (`scheme://user:pass@`);
- masks hex runs of 32 or more characters, and base64-like runs of 40 or more;
- masks known secret values: the value of any variable in the host's environment
  whose name contains `TOKEN`, `SECRET`, `PASSWORD`, `KEY` or `CREDENTIAL`, and is 8
  or more characters long, wherever it appears.

**Nothing environment-wide is dumped.** The environment is never written into the
diagnostic; it is only consulted to mask known secret values. Ordinary context such
as `PATH` or `HOME` is not masked.

**Best-effort, stated plainly.** Masking runs on the snapshotted text, after
truncation, so chunk boundaries cannot split a shape it handles. A secret in a form
it does not recognise is not masked.

### 4. The failure event carries it

When `report_failure` fires, at the existing threshold and under the existing
one-unacked-failure-row policy, its text carries, for the most recent failed run:
- the sanitized command;
- the count of stops in a row;
- the exit code, signal or launch error;
- the sanitized stderr tail.

The event is the existing durable logged failure event, so it survives restarts and
the next `Running` lifecycle. Its lifecycle, acknowledgement and reply obligations
are unchanged.

### 5. What is unchanged

Restart, backoff, the failure threshold, the one-unacked-failure-row policy, the
ingest lease, stdout ingestion, and owed-work behaviour.

## Acceptance

- **The cause is named.**
  - A producer that writes to stderr and exits with code 3 each run: the raised
    event contains "exit 3" and its last stderr lines, including one written
    immediately before exit.
  - A run killed by a signal names the signal.
  - A launch failure names its error.
- **Bounds:**
  - megabytes in ONE unterminated line keep only the 4 KiB tail, with memory
    bounded;
  - invalid UTF-8 and control bytes are rendered escaped;
  - EOF without a newline is kept;
  - the rendered diagnostic stays within 8 KiB after escaping.
- **The machine path is bounded.** Through a non-terminal (piped) wait, a producer
  writing many short stderr lines:
  - echoes nothing, so the collected stderr stays bounded;
  - is not blocked, because its pipe is still drained;
  - and its failure event still carries the sanitized tail.

  The one-unterminated-line case above covers the other allocation boundary.
- **One cutoff:** stdout and stderr finish within the same root-exit window, not one
  after the other.
- **Run ownership:**
  - successive runs with different stderr do not contaminate each other;
  - a descendant holding stderr open does not delay restart, and its later writes
    are not attributed to the next run;
  - the reader stops at the run's end.
- **Redaction:**
  - `token=…`, `Authorization: Bearer …` and URL userinfo are masked in stderr, in
    argv, and in a launch error;
  - a host secret variable's value is masked wherever it appears, while `PATH` and
    `HOME` values are not;
  - the echo is sanitized and line-capped.
- **Durability:** the logged diagnostic is present after the producer restarts.
- **Unchanged policy:** one unacked failure row; the threshold; restart and backoff;
  the ingest lease; ack behaviour. The existing tests of these still pass.

## As built

- **`cli::source_diagnostic`:**
  - `Tail` keeps a run's last 4 KiB of raw stderr. `render` decodes lossily, drops a
    leading line cut by truncation, keeps the last 20 lines, sanitizes each, and caps
    the result at 8 KiB.
  - `drain` reads stderr in 4 KiB chunks into the tail. When given an `Echo` (name
    and writer), it echoes lines through a 1 KiB line buffer, cutting longer lines,
    each sanitized.
  - `Ended::of` takes an `ExitStatus`: exit code, else signal, else unknown. A launch
    failure is `Ended::Launch`.
  - `failure_text` composes the event's text: the sanitized command, the streak, how
    the last run ended, and the stderr tail.
- **`sanitize`:**
  - masks host secret values, and the masking patterns the plan lists;
  - then escapes control characters.

  `host_secrets` takes the values of environment variables whose names contain
  TOKEN, SECRET, PASSWORD, KEY or CREDENTIAL, 8 characters or longer, longest first.
- **The host** (`wait.rs`, `run_leased_command_source`):
  - each run makes its own `Tail` and stderr reader;
  - the echo is enabled only when the wait's stderr `is_terminal()`;
  - the root-exit window is one absolute `cutoff`, used by the stdout drain and then
    the stderr reader, which is aborted if it is still running at the cutoff;
  - at `FAILURES_BEFORE_SURFACING`, `report_failure` gets `failure_text` for the last
    run.

  Restart, backoff, the threshold, the one-unacked-failure policy, leases and stdout
  ingestion are untouched.
- **Tests:**
  - `source_diagnostic`:
    - the tail keeps only its last bytes (a 3 MiB single line, invalid UTF-8,
      control bytes, a missing final newline, more than 20 lines, the 8 KiB cap);
    - secrets are masked wherever they appear, while `PATH` and `HOME` are not;
    - the failure text gives exit, signal or launch, with argv and launch-error
      secrets masked;
    - the echo stays off unless asked for (a flood of short lines echoes nothing and
      keeps the tail) and is capped and sanitized when on.
  - `wait`:
    - through the real host, the failure after three runs names "exit 3" and run 3's
      masked tail, with nothing from the flooding run or run 2's descendant, and the
      restarts are not delayed by a descendant holding stderr;
    - a signalled run names its signal.
- **Mutation checks:** five, all caught, including waiting the stderr reader out
  past the cutoff and sharing one tail across runs.
- **Corrections after review:**
  - **Quoted values:** after `token=` (or `password=`, `secret=`, `api_key=`), a
    quoted value is masked as a whole, up to its closing quote or, if cut off, the
    end of the line. The quotes and the surrounding context are kept.
  - **Environment scan:** `host_secrets` reads the environment with `vars_os()`,
    lossily, so a variable that is not Unicode can no longer panic the host and stop
    the source.
  - **The stderr reader is a `Reader` owned by its run.** Dropping it, on any return
    or when the host future is cancelled, aborts the task and releases the pipe.
    `finish_by` lets it read until the shared cutoff, then aborts it and awaits its
    end before the tail is snapshotted.
  - **Tests:**
    - a quoted value is masked whole in argv, launch errors and stderr, including an
      unterminated one;
    - a non-Unicode value is read lossily;
    - dropping a reader releases its pipe;
    - finishing one past its cutoff stops it, bounded.
  - **Mutation checks:**
    - a `Drop` that detaches instead of aborting;
    - a quoted value left unmasked;
    - a `finish_by` that never stops.

    All three are caught.
  - **Not tested end to end:** host cancellation and the sink-closed early return.
    The `Reader` is a value owned by the run's future, so both paths drop it, and the
    drop is what the guard test covers.
- **Second round of corrections:**
  - **The echo never blocks.** It writes only through `NonBlocking`: the terminal
    opened afresh from `ttyname_r(2)` with `O_NONBLOCK | O_NOCTTY`, so the mode is
    its own and not the shared stderr's. A line the terminal cannot take now is
    dropped. Nothing synchronous in the reader can block, so `finish_by`'s abort
    and await end within the cutoff. The std stderr lock is no longer used.
  - **Escaped quotes:** `closing_quote` finds the first quote not escaped by a
    backslash, tracking escape parity, or the end of the line when the value never
    closes. A value like `"abc\"hunter2"` is masked whole, and the context after it
    is kept.
  - **Tests:**
    - a stalled echo (a non-blocking pipe never read) cannot hold the reader past
      its cutoff, and the pipe is released after;
    - escaped quotes, escaped backslashes and an unterminated escape are handled;
    - the echo tests now go through a non-blocking pipe.
  - **Mutation checks:** "the echo blocks" (no `O_NONBLOCK`) hangs the stalled-echo
    test, and is caught by its timeout. "Escapes ignored" fails the escaped-quote
    test.
