# remote-is-a-row-under-the-agents

> I need a button. Just under the agent list. It should have the URL
> to visit if it's running. — lloyd

## A key nobody can see

`r` switches the remote on and off, and the bar's `⌁` says which —
but nothing on the screen says there is an `r`, and the URL is
shown once, in an overlay that closes. A switch belongs where the
other controls are: a row in the panel, under the agents, that
reads its state and holds its URL.

## The design

A selectable row after `+ add agent`, before the stash and queue
rows, in the panel's own idiom — glyph, name, dim detail:

    ⌁ remote  off
    ⌁ remote  starting…
    ⌁ remote  http://127.0.0.1:8088
    ⌁ remote  failed — cannot listen on 127.0.0.1:8088: address in use

The glyph in the hue the bar gives it. Enter or Space on the row is
the switch (the same `Remote::toggle`); `o` on the row when it is on
opens the URL in the browser again; `r` stays as the shortcut from
the log view. The bar's glyph stays — it is the one place visible
from every page. The overlay stays for the two moments it earns: a
start that could not begin, and a server that ended. The listening
overlay goes: the row now says the URL for as long as it is on, and
the browser opens by itself.

`PanelRow::Remote` joins the row model; `panel_rows` places it; the
Enter/Space/`o` routing gives it `PanelAction::ToggleRemote` and
`PanelAction::OpenRemote`. `PanelView` carries the URL beside the
state so the row can say it. The "failed" reason is the notice's
message, kept on the `Remote` so the row can show it after the
overlay is gone.

## Tests

- `panel_rows`: the remote row sits after `+ add`, before stash and
  queue; cursor movement crosses it.
- `agent_panel_action`: Enter and Space on it toggle; `o` on it opens
  when on and is inert when off; `o` elsewhere is unchanged.
- render: each state's row text; the URL is on the row when on; the
  reason when failed.
- `Remote`: the failure reason is kept and readable; the listening
  outcome no longer yields a notice.
- Mutations: the row placed before `+ add` — caught; `o` opening when
  off — caught; the reason dropped — caught.

## Out of scope

- A port setting, remote reach beyond localhost.

## Acceptance

- [ ] a `remote` row under the agent list shows off / starting / the
      URL / the failure reason
- [ ] Enter or Space on it switches the server on and off; `o` opens
      the URL again
- [ ] `r` and the bar glyph still work
- [ ] tests as above, mutation-checked
