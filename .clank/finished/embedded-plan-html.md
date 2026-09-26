# embedded-plan-html
# Keep plan pages inside the agent website

## Goal

Opening a plan HTML page must keep the agent website chrome and navigation available, so the user can return to agents without leaving the remote UI.

## Work

Reuse or extract the plan HTML content inside a web view that retains the site navigation. Preserve plan links and readable formatting, and give the user an obvious route back to the agent view.

## Acceptance

An automated browser fixture selects a plan and checks that its content appears inside the agent website while the site chrome and a working route back to the team remain available. The fixture checks that plan links and readable formatting survive and that selection does not navigate the top-level page away from the remote UI. No person at the keyboard is required.
