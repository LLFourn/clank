# the-top-bar-names-the-project
# The top bar names the project

## Why

> "we need to have the project name in the top bar and it needs to
> [say] the page name metadata."

Neither is there. The top bar says the agent and what it owes; nothing
on the page says WHICH REPO any of it is about. Open the remote for two
repos and the two pages are indistinguishable — same layout, same agent
names, same everything.

The browser tab is worse: `document.title` is the zellij SESSION name,
which is `clank-clank` here. That is the multiplexer's name for a
window, not the name of the project, and it is what a bookmark, a tab
and a home-screen icon all end up carrying.

The reference does this in the obvious place — the conversation's name
on top, the project under it — and that is the one piece of it we left
out.

## The model

**A page about a repo says which repo.**

`StatusSnapshot` has carried `basename` all along; `web_facts` simply
never passed it on. So this is one field, not a mechanism: the facts
gain the project, and the two places that should have been saying it
say it.

The session name stays where it is, in the ledger's heading. It answers
a different question — which zellij session is serving this — and it is
the right answer to that question and the wrong one to "what is this".

## Deliverables

1. **`WebFacts.project`**, from `snap.basename`.
2. **The top bar names it**, before the agent: the project muted and
   the agent in its own weight, so the row reads project-then-agent
   without becoming two rows. The picker, the caret and `···` are
   unchanged.
3. **`document.title` is the project.** It is what the tab, the
   bookmark and the phone's home screen show. The
   `window.CLANK_SESSION || 'clank'` expression STAYS exactly as it is
   — `web/mod.rs` string-replaces it and the substitution has a test —
   but what it feeds is the ledger heading alone.
4. **A title before the first frame.** The page is served and rendered
   before any facts arrive; until then the tab says `clank` rather
   than flashing a session name it is about to replace.

## Tests

- `web_facts` carries the snapshot's basename.
- The served page still compiles the session in, and the anchor still
  exists — the existing test covers it and must keep passing.
- In the browser: the project appears in the top bar, the tab title
  becomes the project when facts arrive, and the ledger still shows the
  session. At 320px the header still fits, which is the width that has
  caught every layout mistake so far.
- Mutation-check each.

## Out of scope

- Anything that names the repo differently from `clank status` — the
  basename is what the TUI and the site already use, and a second
  opinion about a repo's name is worse than none.
- The ledger's contents beyond its heading.
