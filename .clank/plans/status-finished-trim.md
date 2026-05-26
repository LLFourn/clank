# status-finished-trim

## Summary

`clank status` shows too many finished plans. Change:
- No active plans → show only the most recently finished plan
- Active plans exist → don't show finished plans at all

## JSON shape

`finished_plans` array is always present but contains at most
the last finished plan when no active plans exist, otherwise
empty.

## Tests

- Active plans: human output has no finished section, JSON
  `finished_plans` is empty.
- No active plans: human shows "last finished: ...", JSON
  `finished_plans` has one entry.
