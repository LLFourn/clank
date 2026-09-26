# timeline-tray-toggle
# Make the timeline control a toggled tray button

## Goal

Replace the timeline ellipsis menu and its separate Close button with a tray button that visibly indicates when the tray is open and toggles it closed on a second press.

## Work

Update the web control, open-state styling, and interaction so the same button opens and closes the timeline. Preserve keyboard and screen-reader behavior.

## Acceptance

An automated browser fixture presses the timeline button twice and checks that the first press opens the tray and marks the button pressed, while the second closes the tray and clears the pressed state. The fixture also checks that there is no separate Close control and that keyboard activation and screen-reader state remain usable. No person at the keyboard is required.
