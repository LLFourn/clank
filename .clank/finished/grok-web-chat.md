# grok-web-chat
# Make Grok usable through the chat interface

## Goal

The Clank chat interface must display and submit messages for a Grok session, without requiring the user to switch to its terminal.

## Work

Trace Grok transcript discovery, message rendering, and submission in the web chat path. Repair whichever part prevents normal chat use, while retaining the terminal as an available view.

## Acceptance

An automated web-chat test over a dummy repo shows a Grok transcript, accepts a submitted message, and displays the resulting reply. A failure names its reason in the interface. The terminal remains available. Verification requires no person at the keyboard.
