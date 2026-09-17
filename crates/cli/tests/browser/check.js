const { open, done, facts, agent, panes, turns } = require('./harness');
const out = [];
const ok = (name, cond, detail = '') => out.push(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);

(async () => {
  try {
  // ---- P1: the draft's recipient survives the roster losing its agent
  {
    const { browser, page } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master', { verb: 'working', object: 'foo', since: Math.floor(Date.now()/1000) - 240 }),
      agent('codex', 'commit'),
    ]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude', 'codex']));
    ok('auto shows the turn holder', await page.textContent('#whoname') === 'claude');
    await page.fill('#text', 'a message for claude');
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('codex', 'commit', { verb: 'reviewing', object: 'abc1230', since: Math.floor(Date.now()/1000) - 60 }),
    ]));
    const who = await page.textContent('#whoname');
    ok('a draft holds the view when its agent leaves', who === 'claude', `header says ${who}`);
    ok('the vanished recipient is explained', (await page.textContent('#doing')).includes('left the roster'));
    ok('the draft can still be delivered while the pane lives', !(await page.isDisabled('#send')));
    await page.evaluate((d) => window.__send('panes', d), panes(['codex']));
    ok('send goes quiet once the pane is gone too', await page.isDisabled('#send'));
    ok('the view still has not moved', await page.textContent('#whoname') === 'claude');
    await browser.close();
  }

  // ---- P2: a successful send releases the draft and auto moves on
  {
    const { browser, page } = await open();
    const now = Math.floor(Date.now()/1000);
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master', { verb: 'working', object: 'foo', since: now - 240 }),
      agent('codex', 'commit'),
    ]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude', 'codex']));
    await page.fill('#text', 'hello');
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master'),
      agent('codex', 'commit', { verb: 'reviewing', object: 'abc1230', since: now - 60 }),
    ]));
    ok('the turn moving does not move the view under a draft', await page.textContent('#whoname') === 'claude');
    await page.click('#send');
    await page.waitForTimeout(80);
    const sent = await page.evaluate(() => window.__frames.find((f) => f[0] === '/say'));
    ok('the message went to the agent it was typed to', sent && JSON.parse(sent[1]).pane === 'pane-claude', sent && sent[1]);
    const after = await page.textContent('#whoname');
    ok('auto resolves as soon as the box is empty', after === 'codex', `header says ${after}`);
    ok('send follows the new recipient', !(await page.isDisabled('#send')));
    await browser.close();
  }

  // ---- P2: the clock is never clipped with the sentence
  {
    const { browser, page } = await open();
    // Long enough to overflow a phone in a PROPORTIONAL font. The
    // fixture that caught the original bug was sized for monospace
    // and stopped overflowing when the page changed font — passing
    // while testing nothing (codex on f100b52).
    const long = 'a-plan-whose-name-runs-on-far-past-the-width-of-any-phone-ever-made';
    await page.evaluate(([obj, since]) => window.__send('status', {
      facts: {
        lamp: 'lamp', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: { verb: 'working', object: obj, since }, last: null, transcript: false }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), [long, Math.floor(Date.now() / 1000) - 240]);
    for (const w of [320, 390, 430]) {
      await page.setViewportSize({ width: w, height: 780 });
      await page.waitForTimeout(30);
      const seen = await page.evaluate(() => {
        const c = document.querySelector('#doing .clock'), s = document.querySelector('#doing .said');
        const box = document.querySelector('#doing');
        const r = c.getBoundingClientRect(), b = box.getBoundingClientRect();
        const sr = s.getBoundingClientRect(), cs = getComputedStyle(s);
        return {
          text: c.textContent, right: Math.round(r.right), edge: Math.round(b.right),
          w: Math.round(r.width), overflows: s.scrollWidth > s.clientWidth,
          clear: Math.round(sr.right) <= Math.round(r.left) + 1,
          // Painted overflow is not observable from the DOM — a box
          // with `overflow: visible` still reports the same geometry
          // while its text runs across the clock. The declaration is
          // the mechanism, so the declaration is what gets asserted.
          hides: cs.overflow === 'hidden' && cs.textOverflow === 'ellipsis',
        };
      });
      ok(`the clock is whole at ${w}px`, seen.w > 0 && seen.right <= seen.edge + 1, JSON.stringify(seen));
      ok(`the sentence gives way at ${w}px`, seen.overflows && seen.hides && seen.clear, JSON.stringify(seen));
      ok(`the clock still says the age at ${w}px`, seen.text === '4m', seen.text);
    }
    // And nothing is clipped when it fits: the ellipsis is a response
    // to the width, not a permanent state.
    await page.setViewportSize({ width: 1400, height: 780 });
    await page.evaluate((since) => window.__send('status', {
      facts: {
        lamp: 'lamp', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: { verb: 'working', object: 'foo', since }, last: null, transcript: false }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), Math.floor(Date.now() / 1000) - 240);
    await page.waitForTimeout(30);
    const roomy = await page.evaluate(() => {
      const s = document.querySelector('#doing .said');
      return s.scrollWidth <= s.clientWidth;
    });
    ok('a short obligation is not clipped when there is room', roomy);
    await browser.close();
  }

  // ---- the picker still opens, marks, dismisses
  {
    const { browser, page } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master', { verb: 'working', object: 'foo', since: Math.floor(Date.now()/1000) - 60 }),
      agent('codex', 'commit'),
    ]));
    await page.click('#who');
    ok('the picker opens with auto first', (await page.textContent('.picker .opt:first-child .nm')) === 'auto');
    ok('auto is marked as the selection', (await page.getAttribute('.picker .opt:first-child', 'aria-selected')) === 'true');
    await page.click('.picker .opt[data-key="pin:codex"]');
    ok('a pin moves the view', await page.textContent('#whoname') === 'codex');
    ok('the auto tag goes away when pinned', await page.isHidden('#autotag'));
    ok('the caret says the turn is elsewhere', await page.isVisible('#who .away'));
    await page.reload();
    await page.waitForFunction(() => !!window.__sock);
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master', { verb: 'working', object: 'foo', since: Math.floor(Date.now()/1000) - 60 }),
      agent('codex', 'commit'),
    ]));
    ok('the pin survives a reload', await page.textContent('#whoname') === 'codex');
    await page.click('#who');
    await page.keyboard.press('Escape');
    ok('Escape dismisses', await page.isHidden('#picker'));
    await browser.close();
  }

  // ---- Stop stands beside Send, never in its place
  {
    const { page, errors } = await open();
    const now = Math.floor(Date.now() / 1000);
    // `ended` is the server's half; the newest TURN is the other, and
    // it arrives on its own cadence.
    const say = (readable, ended) => page.evaluate(([r, e]) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: null, last: null, transcript: r, ended: e }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), [readable, ended]);
    const spoke = (at, id) => page.evaluate(([a, i]) => window.__send('turn', {
      agent: 'claude', session: 's1', generation: 1,
      turn: { id: i, who: 'agent', at: a, body: { kind: 'text', text: 'x', html: '<p>x</p>' } },
    }), [at, id]);

    await say(true, now);
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await page.evaluate((d) => window.__send('turns', d), turns('claude', []));
    ok('a stamp with nothing after it is idle', await page.isHidden('#stop'));

    // The case that made this evidence rather than a verdict: a turn
    // arrives with NO new status frame behind it.
    await spoke(now + 5, 't-live');
    ok('a turn alone brings Stop back', await page.isVisible('#stop'),
       'no status frame followed it');
    ok('and Send is untouched', !(await page.isDisabled('#send')));

    await say(true, now + 5);
    ok('a tie is not evidence of work', await page.isHidden('#stop'));
    await say(false, now);
    ok('no transcript adapter means doubt, which shows Stop', await page.isVisible('#stop'));
    await say(true, null);
    ok('no stamp means doubt too', await page.isVisible('#stop'));

    // Pressing Stop settles the button, because a harness may never
    // report the end.
    await say(true, now);
    await spoke(now + 9, 't-run');
    ok('working again', await page.isVisible('#stop'));
    await page.click('#stop');
    await page.waitForTimeout(40);
    const sent = await page.evaluate(() => window.__frames.find((f) => f[0] === '/stop'));
    ok('Stop reaches the pane of the agent on screen', sent && JSON.parse(sent[1]).pane === 'pane-claude', sent && sent[1]);
    ok('and the button settles though the evidence still says working', await page.isHidden('#stop'));
    ok('Send survived all of it', !(await page.isDisabled('#send')));
    await spoke(now + 12, 't-again');
    ok('a turn from the agent re-arms Stop', await page.isVisible('#stop'));

    // A harness with NO transcript never sends a turn, so sending it
    // work has to re-arm the button by itself.
    await say(false, null);
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('an agent with no transcript can be stopped', await page.isHidden('#stop'));
    await page.fill('#text', 'do something else');
    await page.click('#send');
    await page.waitForTimeout(60);
    ok('and sending it work re-arms Stop with no transcript at all', await page.isVisible('#stop'));

    // A stop that FAILED leaves the agent running, so the button has
    // to come back for a retry.
    await page.evaluate(() => { window.fetch = async (u) => { window.__frames.push([u, null]); return { ok: false, status: 502, text: async () => 'boom' }; }; });
    await page.click('#stop');
    await page.waitForTimeout(60);
    ok('a failed Stop can be retried', await page.isVisible('#stop'));
    ok('and says so', (await page.textContent('#sent')).includes('Not stopped'));
    ok('no errors around Stop', errors.length === 0, errors.join('; '));
  }

  // ---- a Stop must not outlive the activity it stopped
  {
    const { page, errors } = await open();
    const say = (ended) => page.evaluate((e) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: null, last: null, transcript: true, ended: e }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), ended);
    const snapshot = (session, gen, ats) => page.evaluate(([s, g, list]) => window.__send('turns', {
      agent: 'claude', session: s, generation: g,
      turns: list.map((at, i) => ({ id: `s${s}-${g}-${i}`, who: 'agent', at, body: { kind: 'text', text: 'x', html: '<p>x</p>' } })),
    }), [session, gen, ats]);

    await say(10);
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await snapshot('s1', 1, [20]);
    ok('working, on a snapshot', await page.isVisible('#stop'));
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('stopped', await page.isHidden('#stop'));

    // A reconnect resends the WHOLE window, so this is how new work
    // often arrives — not as an increment.
    await snapshot('s1', 1, [20]);
    ok('an identical replay does not undo a deliberate Stop', await page.isHidden('#stop'));
    await snapshot('s1', 1, [20, 30]);
    ok('a snapshot carrying new work re-arms Stop', await page.isVisible('#stop'));

    // A new incarnation can land in the SAME pane, so the pane alone
    // cannot say whether this is the run that was stopped.
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('stopped again', await page.isHidden('#stop'));
    await snapshot('s2', 1, [30]);
    await say(null);
    ok('a new session in the same pane re-arms Stop', await page.isVisible('#stop'));

    // Two turns can share a second, and a harness may send no
    // timestamp at all — so the clock cannot be what tells activity
    // apart (codex on e14c244). The page knows the SET changed.
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('stopped once more', await page.isHidden('#stop'));
    await snapshot('s2', 1, [30, 30]);
    ok('a new turn in the same second re-arms Stop, on a snapshot', await page.isVisible('#stop'));

    await page.click('#stop');
    await page.waitForTimeout(40);
    await page.evaluate(() => window.__send('turn', {
      agent: 'claude', session: 's2', generation: 1,
      turn: { id: 'same-second', who: 'agent', at: 30, body: { kind: 'text', text: 'x', html: '<p>x</p>' } },
    }));
    await page.waitForTimeout(40);
    ok('and incrementally too', await page.isVisible('#stop'));

    await page.click('#stop');
    await page.waitForTimeout(40);
    await page.evaluate(() => window.__send('turn', {
      agent: 'claude', session: 's2', generation: 1,
      turn: { id: 'no-clock', who: 'agent', at: null, body: { kind: 'text', text: 'x', html: '<p>x</p>' } },
    }));
    await page.waitForTimeout(40);
    ok('a turn with no timestamp re-arms Stop', await page.isVisible('#stop'));

    // A tool's output arriving is activity too, on the same turn id.
    await page.evaluate(() => window.__send('turn', {
      agent: 'claude', session: 's2', generation: 1,
      turn: { id: 'tool-1', who: 'agent', at: 31, body: { kind: 'tool', name: 'Bash', input: 'sleep 30', output: null, images: [] } },
    }));
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('stopped with a tool in flight', await page.isHidden('#stop'));
    await page.evaluate(() => window.__send('turn', {
      agent: 'claude', session: 's2', generation: 1,
      turn: { id: 'tool-1', who: 'agent', at: 31, body: { kind: 'tool', name: 'Bash', input: 'sleep 30', output: 'done', images: [] } },
    }));
    await page.waitForTimeout(40);
    ok('its output arriving re-arms Stop, same id and clock', await page.isVisible('#stop'));

    // An agent with no transcript can only be re-armed by something
    // outside it: a new pane is the harness having been restarted,
    // which is new work by definition.
    await page.evaluate((d) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'kimi', role: 'commit', owes: null, last: null, transcript: false, ended: null }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }, d), null);
    await page.evaluate(() => window.__send('panes', { panes: [{ id: 'pane-kimi', label: 'kimi', columns: 80, rows: 24, exited: false }], closed: [] }));
    ok('an agent with no transcript offers Stop', await page.isVisible('#stop'));
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('and can be stopped', await page.isHidden('#stop'));
    await page.evaluate(() => window.__send('panes', { panes: [{ id: 'pane-kimi-2', label: 'kimi', columns: 80, rows: 24, exited: false }], closed: [] }));
    ok('a fresh pane re-arms Stop even with no transcript', await page.isVisible('#stop'));

    ok('no errors around suppression', errors.length === 0, errors.join('; '));
  }

  // ---- the attach control puts a path in the box
  {
    const { page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master')]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await page.evaluate(() => {
      window.fetch = async (url, opts) => {
        window.__frames.push([url, opts && opts.headers && opts.headers['content-type']]);
        if (url === '/upload') return { ok: true, status: 200, json: async () => ({ path: '/repo/.clank/attachments/1700-abc.png' }) };
        return { ok: true, status: 204, text: async () => '' };
      };
    });
    await page.setInputFiles('#file', { name: 'shot.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForTimeout(60);
    ok('the file was uploaded with its type', await page.evaluate(() => window.__frames.some((f) => f[0] === '/upload' && f[1] === 'image/png')));
    ok('and the path is in the box, as editable text',
       (await page.inputValue('#text')) === '/repo/.clank/attachments/1700-abc.png',
       await page.inputValue('#text'));

    // A second attachment joins the first rather than replacing it,
    // and words already typed survive.
    await page.fill('#text', 'look at this');
    await page.setInputFiles('#file', { name: 'b.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForTimeout(60);
    ok('an attachment joins what was already typed',
       (await page.inputValue('#text')) === 'look at this /repo/.clank/attachments/1700-abc.png',
       await page.inputValue('#text'));

    // A refused upload must not put a path in the box.
    await page.fill('#text', '');
    // The refusal stub still answers `json`, or dropping the `ok`
    // check would throw for the wrong reason and this would pass
    // while testing nothing.
    await page.evaluate(() => { window.fetch = async () => ({ ok: false, status: 413, text: async () => 'too big', json: async () => ({ path: '/repo/.clank/attachments/should-not-appear.png' }) }); });
    await page.setInputFiles('#file', { name: 'huge.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForTimeout(60);
    ok('a refused upload leaves the box alone', (await page.inputValue('#text')) === '');
    ok('and says so', (await page.textContent('#sent')).includes('Not attached'));
    ok('no errors around attaching', errors.length === 0, errors.join('; '));
  }

  // ---- an upload on its way is part of the draft
  {
    const { page, errors } = await open();
    const now = Math.floor(Date.now() / 1000);
    const two = (workingClaude) => page.evaluate(([n, w]) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [
          { label: 'claude', role: 'master', owes: w ? { verb: 'working', object: 'x', since: n - 9 } : null, last: null, transcript: false, ended: null },
          { label: 'codex', role: 'commit', owes: w ? null : { verb: 'reviewing', object: 'y', since: n - 3 }, last: null, transcript: false, ended: null },
        ],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), [now, workingClaude]);

    await two(true);
    await page.evaluate((d) => window.__send('panes', d), panes(['claude', 'codex']));
    ok('auto is showing claude', (await page.textContent('#whoname')) === 'claude');

    // An upload that will not answer until we say so.
    await page.evaluate(() => {
      window.__resolve = null;
      window.fetch = (url, opts) => {
        window.__frames.push([url, opts && opts.body]);
        if (url === '/upload') return new Promise((res) => { window.__resolve = () => res({ ok: true, status: 200, json: async () => ({ path: '/repo/.clank/attachments/p.png' }) }); });
        return Promise.resolve({ ok: true, status: 204, text: async () => '' });
      };
    });
    await page.setInputFiles('#file', { name: 'p.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForFunction(() => !!window.__resolve);

    // The turn moves while the file is still going up.
    await two(false);
    ok('an upload in flight holds the view', (await page.textContent('#whoname')) === 'claude',
       'or the photo lands in the wrong agent\'s box');
    await page.evaluate(() => window.__resolve());
    await page.waitForTimeout(60);
    ok('the path landed in the box it was started from', (await page.inputValue('#text')).includes('p.png'));
    ok('and the view is still on that agent', (await page.textContent('#whoname')) === 'claude');

    // Once the box empties, auto is free to follow the turn again.
    await page.fill('#text', '');
    await page.evaluate(() => window.dispatchEvent(new Event('x')));
    await page.evaluate(() => document.querySelector('#text').dispatchEvent(new Event('input')));
    await page.waitForTimeout(40);
    ok('and moves once the draft is gone', (await page.textContent('#whoname')) === 'codex');
    ok('no errors around the pending upload', errors.length === 0, errors.join('; '));
  }

  // ---- a send and an upload cannot silently eat each other
  {
    const { page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master')]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    // Both requests deferred, resolvable in either order.
    await page.evaluate(() => {
      window.__up = null; window.__say = null; window.__said = []; window.__n = 0;
      window.fetch = (url, opts) => {
        // A distinct path per upload, as the server gives.
        if (url === '/upload') { const n = ++window.__n; return new Promise((res) => { window.__up = () => res({ ok: true, status: 200, json: async () => ({ path: `/repo/.clank/attachments/f${n}.png` }) }); }); }
        if (url === '/say') { window.__said.push(JSON.parse(opts.body).text); return new Promise((res) => { window.__say = () => res({ ok: true, status: 204, text: async () => '' }); }); }
        return Promise.resolve({ ok: true, status: 204, text: async () => '' });
      };
    });

    // Upload first, then Send: the message must not leave without it.
    await page.fill('#text', 'look at this');
    await page.setInputFiles('#file', { name: 'photo.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForFunction(() => !!window.__up);
    ok('Send waits while a file is still arriving', await page.isDisabled('#send'));
    // A disabled button stops a click and stops implicit submission,
    // but not a script — and the handler is where the invariant
    // lives, so that is what gets tested.
    await page.evaluate(() => document.querySelector('#say').requestSubmit());
    await page.waitForTimeout(40);
    ok('and a submit that goes round the button is refused', await page.evaluate(() => window.__said.length) === 0);
    ok('which it says', (await page.textContent('#sent')).includes('Still attaching'));
    await page.evaluate(() => window.__up());
    await page.waitForTimeout(60);
    ok('once it lands the path is in the box', (await page.inputValue('#text')).includes('f1.png'));
    await page.click('#send');
    await page.waitForTimeout(40);
    const carried = await page.evaluate(() => window.__said[0]);
    ok('the sent text names the attachment', carried === 'look at this /repo/.clank/attachments/f1.png', carried);
    await page.evaluate(() => window.__say());
    await page.waitForTimeout(40);
    ok('the box is empty after it goes', (await page.inputValue('#text')) === '');

    // The other order: a file attached WHILE a send is in flight.
    await page.evaluate(() => { window.__up = null; window.__say = null; });
    await page.fill('#text', 'first message');
    await page.click('#send');
    await page.waitForTimeout(40);
    await page.setInputFiles('#file', { name: 'later.png', mimeType: 'image/png', buffer: Buffer.from('PNG') });
    await page.waitForFunction(() => !!window.__up);
    ok('the box is the next message as soon as it is dispatched',
       (await page.inputValue('#text')) === '', await page.inputValue('#text'));
    await page.evaluate(() => window.__up());
    await page.waitForTimeout(60);
    ok('an attachment landing mid-send waits in the box',
       (await page.inputValue('#text')).includes('f2.png'), await page.inputValue('#text'));
    ok('and does not re-open Send while one is still in flight', await page.isDisabled('#send'),
       'or the same instruction goes twice');
    await page.evaluate(() => document.querySelector('#say').requestSubmit());
    await page.waitForTimeout(40);
    ok('nor can a scripted submit', await page.evaluate(() => window.__said.length) === 2);
    await page.evaluate(() => window.__say());
    await page.waitForTimeout(40);
    const second = await page.evaluate(() => window.__said[1]);
    ok('what was sent is what was captured', second === 'first message', second);
    ok('and the attachment is untouched by the answer',
       (await page.inputValue('#text')) === '/repo/.clank/attachments/f2.png',
       await page.inputValue('#text'));

    // A replacement draft that happens to share a prefix with the
    // message in flight is not the message in flight.
    await page.fill('#text', '');
    await page.evaluate(() => { window.__say = null; });
    await page.fill('#text', 'hi');
    await page.click('#send');
    await page.waitForTimeout(40);
    await page.fill('#text', 'history matters');
    await page.evaluate(() => window.__say());
    await page.waitForTimeout(40);
    ok('a shared prefix is not ownership', (await page.inputValue('#text')) === 'history matters',
       await page.inputValue('#text'));

    // A send that fails gives its words back, in front of anything
    // written since.
    await page.fill('#text', '');
    await page.evaluate(() => {
      window.fetch = (url, opts) => {
        if (url === '/say') { window.__said.push(JSON.parse(opts.body).text); return Promise.resolve({ ok: false, status: 502, text: async () => 'no' }); }
        return Promise.resolve({ ok: true, status: 204, text: async () => '' });
      };
    });
    await page.fill('#text', 'please run the tests');
    await page.click('#send');
    await page.waitForTimeout(60);
    ok('a failed send gives the words back', (await page.inputValue('#text')) === 'please run the tests',
       await page.inputValue('#text'));
    ok('and says so', (await page.textContent('#sent')).includes('Not sent'));
    ok('no errors around the two races', errors.length === 0, errors.join('; '));
  }

  // ---- the composer fits a phone
  {
    const { page, errors } = await open();
    const now = Math.floor(Date.now() / 1000);
    await page.evaluate((n) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: { verb: 'working', object: 'a-plan-whose-name-runs-on-and-on', since: n - 300 }, last: null, transcript: true, ended: n }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), now);
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await page.evaluate((d) => window.__send('turns', d), turns('claude', []));
    await page.evaluate((n) => window.__send('turn', { agent: 'claude', session: 's1', generation: 1,
      turn: { id: 'live', who: 'agent', at: n + 30, body: { kind: 'text', text: 'x', html: '<p>x</p>' } } }), now);
    for (const w of [320, 390, 430]) {
      await page.setViewportSize({ width: w, height: 800 });
      await page.waitForTimeout(40);
      const m = await page.evaluate(() => {
        const right = (s) => Math.round(document.querySelector(s).getBoundingClientRect().right);
        return { doc: document.documentElement.scrollWidth, stop: !document.querySelector('#stop').hidden,
                 send: right('#send'), attach: right('#attach') };
      });
      ok(`Stop is up at ${w}px`, m.stop, JSON.stringify(m));
      ok(`nothing overflows the screen at ${w}px`, m.doc <= w, JSON.stringify(m));
      ok(`Send is reachable at ${w}px`, m.send <= w, JSON.stringify(m));
    }
    ok('no errors around the narrow composer', errors.length === 0, errors.join('; '));
  }

  // ---- a browser that refuses storage still gets a page
  try {
    const { page, errors: errs } = await open(true);
    await page.evaluate((d) => window.__send('status', d), facts([
      agent('claude', 'master', { verb: 'working', object: 'foo', since: Math.floor(Date.now()/1000) - 60 }),
    ]));
    ok('storage refused: the page still runs', errs.length === 0, errs.join('; '));
    ok('storage refused: the header still fills', await page.textContent('#whoname') === 'claude');
    await page.click('#who');
    ok('storage refused: the picker still opens', await page.isVisible('#picker'));
  } catch (err) {
    ok('storage refused: the page still runs', false, err.message.split('\n')[0]);
  }

  } catch (err) {
    // A crash is a failed check, not a quiet exit: an assertion that
    // cannot run has told you something, and grepping for FAIL must
    // see it.
    ok('the run completed', false, String(err).split('\n')[0]);
  }
  try { await done(); } catch (err) {}
  console.log(out.join('\n'));
  process.exit(out.some((l) => l.startsWith('FAIL')) ? 1 : 0);
})();
