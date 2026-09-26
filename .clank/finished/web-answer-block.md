# web-answer-block
# Answer Clank blocks from the web interface

## Goal

When an agent raises a block, the web interface must let the user read and answer it directly.

## Work

Show pending block questions with their agent and context. Add an answer form that uses the same unblock operation and validation as the CLI, and update the displayed state after submission.

## Acceptance

Automated browser and dummy-repo fixtures show a pending block's question, agent, and context. An answer uses the CLI's unblock write and validation, is recorded once, leaves the pending list, and produces the normal unblocked wake that a dummy repo can observe without a live session. A second answer or invalid answer explains the refusal and writes nothing further. No person at the keyboard is required.
