# pending-chat-submission
# Show submitted chat messages while delivery is pending

## Goal

When a user submits a chat message, show it immediately in a pending state until it appears in the agent transcript.

## Work

Track each outgoing message from submit through the send response and the bound agent's transcript. Show it immediately as pending; a successful send response alone does not prove the agent recorded it. Reconcile incoming person turns in order, including repeated identical text, so each submission settles once without a duplicate. Show a failed send beside its message and keep retry possible. Preserve pending messages across a refresh for the same agent/session when browser storage is available; a storage failure must not break sending.

## Acceptance

Automated browser and dummy-repo tests show the submitted message immediately, keep it pending after send succeeds, show a failure reason and retry path when sending fails, and replace it with one settled transcript copy when recorded. Repeated identical submissions settle independently. A refresh retains pending state for the same agent/session when storage works and never attaches it to another session. Verification requires no person at the keyboard or live agent login.
