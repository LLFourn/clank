// The decisions the message box makes, apart from the DOM that acts
// on them.
//
// Values in, verdicts out: no element, no layout read, no event. The
// page measures and applies; what a keypress MEANS and how tall the
// box SHOULD be are decided here, where a test can run them.
//
// Inlined into the session page at its composer anchor, so this text
// is what ships; the tests run this file.

// What a keypress on the composer means: 'send', 'newline' or 'pass'.
//
// `hasKeyboard` is the caller's answer to "is there a physical
// keyboard here", and it is what stops the two promises contradicting
// each other. Enter-sends is a desktop convention; on a touch
// keyboard the return key IS the newline key, and binding it to send
// would leave a phone with no way to type a second line at all --
// which is the device this page is mostly read on (codex a664d6c).
// There, Enter inserts and the Send button sends.
//
// An Enter that CONFIRMS an IME composition is never a send. The
// composer is choosing characters, not finishing a message, and
// sending there would cut a sentence off mid-word in every language
// that needs one.
function composerKey(ev) {
  if (!ev || ev.key !== 'Enter') return 'pass';
  if (ev.isComposing || ev.keyCode === 229) return 'pass';
  if (ev.shiftKey || ev.altKey || ev.ctrlKey || ev.metaKey) return 'newline';
  return ev.hasKeyboard ? 'send' : 'newline';
}

// How tall the box should be, in px, for content that measures
// `contentPx`.
//
// One row at rest; grows with the content; stops at a third of the
// viewport and scrolls inside itself after that. SHRINKS too -- a box
// that only ever grew would be the reported bug pointing the other
// way, so this is a function of the content rather than a maximum
// anything remembers.
//
// The cap follows the viewport because the viewport changes: a phone
// rotating, or a software keyboard opening, leaves a third of the
// screen a different number of pixels, and no input event says so.
function composerHeight(m) {
  const line = Math.max(1, m.lineHeightPx || 0);
  const cap = Math.max(line, Math.floor((m.viewportPx || 0) / 3));
  const wanted = Math.max(line, m.contentPx || 0);
  return Math.min(wanted, cap);
}

// Whether the box needs a scrollbar: only once it has stopped growing.
function composerScrolls(m) {
  return (m.contentPx || 0) > composerHeight(m);
}

// The height of the viewport the reader can actually SEE.
//
// `innerHeight` is the wrong number on a phone with the keyboard
// open, which is the exact case this box exists for: mobile browsers
// shrink only the VISUAL viewport, leaving `innerHeight` at its
// pre-keyboard value. Sizing against it lets a long message keep a cap
// computed for a screen that is no longer visible, and fill what is
// left (codex d36f50e).
//
// Taken as a function of the window so a test can hand it one.
// A reported height of ZERO falls through to the window's, rather
// than capping the box at nothing.
function composerViewport(win) {
  if (!win) return 0;
  const visual = win.visualViewport && win.visualViewport.height;
  return visual || win.innerHeight || 0;
}
