# web-auto-mode-control
# Show and change agent auto mode in the web interface

## Goal

Show whether each agent has auto mode on or off, and let the user change it from the web interface.

## Work

Expose the effective auto-mode state, including inherited defaults, and use the same update semantics as `clank auto`. Update the interface promptly when the state changes and report failures.

## Acceptance

Automated browser and dummy-repo fixtures show explicit on, explicit off, and inherited mode for each agent. A change uses the same per-agent write as `clank auto`, and the effective state shown by the page agrees with the CLI. A refused write explains why and leaves the displayed mode unchanged. No person at the keyboard or live agent login is required.
