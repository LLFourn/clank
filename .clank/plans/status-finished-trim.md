# status-finished-trim

## Summary

`clank status` shows too many finished plans. Change:
- No active plans → show only the most recently finished plan
- Active plans exist → don't show finished plans at all

## Implementation

`crates/cli/src/cli/status.rs`: `print_human` and `build_json`
conditionally render finished plans based on whether there are
active plan views.
