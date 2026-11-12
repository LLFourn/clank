# runtime-shutdown-test-waits-for-its-descendant
# Runtime shutdown waits for its test descendant

The full CLI library suite during wake-message review passed 3557 tests but failed
`proc_run::tests::a_runtime_shutdown_leaves_no_zombie`: no descendant PID file.
Log: /tmp/clank-wake-text/monitor-correction.log.

The test reads only the leader PID before dropping the Tokio runtime, then reads
the descendant PID after shutdown. Inspect the process-tree fixture and establish
that the descendant exists before triggering shutdown. Preserve assertions that
the leader is reaped and the descendant is gone. Do not mask the race with sleeps,
retries, or looser assertions, and add no subprocess dependencies.

Validate the affected test repeatedly and run the full CLI library suite. Keep
this a small test correction with no production behavior changes.
