# Clank release checklist

This stub is to develop a full release checklist. What do we need to move into production:

The main thing we need to do is to test the first experience with it.

1. How are you meant to easily install it -- curl from github to | sh?
2. Need github builds for platforms then
3. I have this sort of hack that is helpful which is to use `say` to do things on hooks -- there's a problem though when you have multiple reviewers it triggers it several times. Need to think through hooks and notifications.
4. With zellij it's not super clear which window is active or what's going on. Could clank zellij fork off another process that uses zellij action. It would be cool to try and make zellij show the currently working window(s) and hide the others until they're active again.
5. A clank tutorial? We need content for a screencast to introduce clank
6. Write a very good README.md that explains clank and gives the motivating pitch.
7. Do we have the right boundries between core and cli -- is the boundry necessary
8. code quality, especially has the core state fold drifted from its original intended design -- a pure fold over commits?
9. When doing clank init there's a warning about not being able to install post rewrite hooks if there's one already there -- is there no way to deal with this cleanly?
10. cli output beauty: clank --help has commands that wrap lines etc. ugly.


The outcome of this plan should be RELEASE-CHECKLIST.md which contains everything we want to do before publicly releasing clank. From that release checklist we will start queuing plans.


## Promote-time notes (2026-06-10)

- **Doc-only plan**: the deliverable is RELEASE-CHECKLIST.md — per
  the FINISHED definition, the finished document IS the
  implementation; reviewers mark FINISHED on the doc, not on code.
- **Shape of the deliverable**: each checklist entry should be
  directly queueable — a name, the problem in 2-3 sentences, and
  enough investigation that turning it into a queue stub is
  mechanical. Items that are pure questions (7: core/cli boundary,
  8: fold drift, 9: hook-install collision) get INVESTIGATED here
  and either answered inline (no follow-up needed) or distilled
  into a concrete work item.
- **Session context to fold in**: item 3 (multi-reviewer `say`
  spam) is partly mitigated — finish-does-not-wake-reviewers cut
  spurious reviewer wakes, and hooks fire from wfw item emission
  (hook_config::run_hook), so the remaining problem is per-agent
  hook multiplicity, not noise volume. Item 4 overlaps the shipped
  zellij.layout template + status --tui (a `zellij action`-driven
  focus follower would be a NEW queue item). Item 10 may be a
  trivial clap `help_template`/width fix — investigate before
  listing it as real work.
