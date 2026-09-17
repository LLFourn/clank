const { open, done, facts, agent, panes } = require('./harness');
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
