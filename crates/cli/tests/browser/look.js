const { open, done, facts, agent, panes, text, tool, turns } = require('./harness');
const out = [];
const ok = (n, c, d = '') => out.push(`${c ? 'PASS' : 'FAIL'}  ${n}${d ? '  — ' + d : ''}`);

(async () => {
  try {
  const { page, errors } = await open();
  await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master', null, null, true)]));
  await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
  await page.evaluate((d) => window.__send('turns', d), turns('claude', [
    text('u1', 'person', 'make it **nice**', '<p>make it <strong>nice</strong></p>'),
    text('a1', 'agent', '# Heading\n\nA paragraph with `code`.',
         '<h1>Heading</h1>\n<p>A paragraph with <code>code</code>.</p>'),
    tool('t1', 'Bash', 'ls -la', 'a\nb'),
    tool('t2', 'Bash', 'grep x', 'found'),
    tool('t3', 'Read', 'file.rs', 'contents'),
    text('a2', 'agent', 'done', '<p>done</p>'),
    tool('t4', 'Bash', 'cargo test', null),
  ]));
  await page.waitForTimeout(60);

  ok('markdown renders as elements', await page.locator('.turn .prose h1').count() === 1);
  ok('inline code is marked up', await page.locator('.turn .prose code').count() === 1);
  ok('three tool calls become one row', await page.locator('.did').count() === 1);
  ok('a run nobody was reading starts closed',
     !(await page.locator('.did').first().evaluate((e) => e.open)));
  ok('the row names how many', (await page.textContent('.did > summary')) === 'Ran 3 commands',
     await page.textContent('.did > summary'));
  ok('every call is still reachable', await page.locator('.did .members .turn.tool').count() === 3);
  ok('a lone in-flight call is not grouped', await page.locator('.turn.tool').count() === 4);
  const running = await page.locator('.turn.tool .pending').last().textContent();
  ok('and says it is running', running.trim() === '…', JSON.stringify(running));

  // a group that grows while in flight
  await page.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
    turn: { id: 't5', who: 'agent', at: Math.floor(Date.now()/1000), body: { kind: 'tool', name: 'Bash', input: 'cargo build', output: null, images: [] } } });
  await page.waitForTimeout(40);
  ok('a second call joins the run', await page.locator('.did').count() === 2);
  const s = await page.locator('.did > summary').last().textContent();
  ok('and the row says it is running', s === 'Running 2 commands…', s);

  // A lone call that becomes a run must not take back what the reader
  // is already looking at.
  {
    const { page: p2, errors: e2 } = await open();
    await p2.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master', null, null, true)]));
    await p2.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await p2.evaluate((d) => window.__send('turns', d), turns('claude', [
      text('p1', 'person', 'go', '<p>go</p>'),
      tool('s1', 'Bash', 'ls', 'THE OUTPUT'),
    ]));
    await p2.waitForTimeout(50);
    await p2.locator('[data-id="s1"] summary').click();
    ok('a lone call opens to its output', await p2.locator('[data-id="s1"] pre.out').isVisible());

    await p2.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
      turn: { id: 's2', who: 'agent', at: Math.floor(Date.now()/1000), body: { kind: 'tool', name: 'Bash', input: 'pwd', output: 'done', images: [] } } });
    await p2.waitForTimeout(50);
    ok('it became a run', await p2.locator('.did').count() === 1);
    ok('and the output the reader had open is STILL visible',
       await p2.locator('[data-id="s1"] pre.out').isVisible());

    // But a group the reader closed stays closed when it grows.
    await p2.locator('.did > summary').click();
    ok('the reader can close the run', !(await p2.locator('.did').first().evaluate((e) => e.open)));
    await p2.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
      turn: { id: 's3', who: 'agent', at: Math.floor(Date.now()/1000), body: { kind: 'tool', name: 'Bash', input: 'id', output: 'x', images: [] } } });
    await p2.waitForTimeout(50);
    ok('and it stays closed as the run grows',
       !(await p2.locator('.did').first().evaluate((e) => e.open)));
    ok('no errors in the disclosure run', e2.length === 0, e2.join('; '));
  }

  // Retention rekeys a run: the group's identity is its first
  // member, and the 200-turn window can evict that member. The
  // replacement must inherit what the reader did to the wrapper it
  // replaces, not guess from what is open inside it.
  {
    const { page: p3, errors: e3 } = await open();
    await p3.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master', null, null, true)]));
    await p3.evaluate((d) => window.__send('panes', d), panes(['claude']));
    const many = [
      tool('r1', 'Bash', 'one', 'OUT ONE'),
      tool('r2', 'Bash', 'two', 'OUT TWO'),
      tool('r3', 'Bash', 'three', 'OUT THREE'),
    ];
    for (let i = 0; i < 197; i++) many.push(text(`f${i}`, 'agent', `line ${i}`, `<p>line ${i}</p>`));
    await p3.evaluate((d) => window.__send('turns', d), turns('claude', many));
    await p3.waitForTimeout(60);

    await p3.locator('.did > summary').click();                 // open the run
    await p3.locator('[data-id="r2"] summary').click();          // open a child
    await p3.locator('.did > summary').click();                  // close the run again
    ok('the reader closed the run, with a child open inside',
       !(await p3.locator('.did').first().evaluate((e) => e.open))
       && (await p3.locator('[data-id="r2"] details').first().evaluate((e) => e.open)));

    // One more turn pushes the window past 200 and evicts r1.
    await p3.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
      turn: { id: 'f197', who: 'agent', at: Math.floor(Date.now()/1000), body: { kind: 'text', text: 'last', html: '<p>last</p>' } } });
    await p3.waitForTimeout(60);
    const key = await p3.locator('.did').first().getAttribute('data-id');
    ok('the run was rekeyed by retention', key === 'g:r2', String(key));
    ok('and it is STILL closed', !(await p3.locator('.did').first().evaluate((e) => e.open)));
    ok('no errors in the retention run', e3.length === 0, e3.join('; '));
  }

  // opened disclosures survive a repaint
  await page.locator('.did').first().click();
  await page.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
    turn: { id: 'a3', who: 'agent', at: Math.floor(Date.now()/1000), body: { kind: 'text', text: 'more', html: '<p>more</p>' } } });
  await page.waitForTimeout(40);
  ok('an opened row stays open across a repaint', await page.locator('.did').first().evaluate((e) => e.open));

  // the measure and the centring
  for (const w of [390, 960, 1400]) {
    await page.setViewportSize({ width: w, height: 900 });
    await page.waitForTimeout(30);
    const m = await page.evaluate(() => {
      const b = document.querySelector('.turns').getBoundingClientRect();
      const host = document.querySelector('.view.transcript').getBoundingClientRect();
      return { w: Math.round(b.width), left: Math.round(b.left - host.left), right: Math.round(host.right - b.right) };
    });
    ok(`a readable measure at ${w}px`, m.w <= 700, JSON.stringify(m));
    if (w > 700) ok(`centred at ${w}px`, Math.abs(m.left - m.right) <= 2, JSON.stringify(m));
  }

  // light page, dark terminal
  const grounds = await page.evaluate(() => {
    const bg = (el) => getComputedStyle(el).backgroundColor;
    return { body: bg(document.body), term: bg(document.querySelector('.view.terminal')) };
  });
  // Transparent is not dark: an unpainted terminal shows the light
  // page through it, which is the exact failure this check exists
  // for — so alpha counts.
  const opaque = (c) => { const p = c.match(/[\d.]+/g).map(Number); return p.length < 4 || p[3] === 1; };
  const lum = (c) => { const [r, g, b] = c.match(/[\d.]+/g).map(Number); return 0.299 * r + 0.587 * g + 0.114 * b; };
  ok('the page is light', opaque(grounds.body) && lum(grounds.body) > 200, grounds.body);
  ok('the terminal keeps its own dark ground', opaque(grounds.term) && lum(grounds.term) < 60, grounds.term);

  // agent prose cannot bring markup of its own
  await page.evaluate((d) => window.__send('turn', d), { agent: 'claude', session: 's1', generation: 1,
    turn: { id: 'x1', who: 'agent', at: Math.floor(Date.now()/1000),
            body: { kind: 'text', text: 'raw', html: '<p>&lt;script&gt;alert(1)&lt;/script&gt; and <a href="https://e.com">ok</a></p>' } } });
  await page.waitForTimeout(40);
  ok('escaped markup shows as text', (await page.textContent('[data-id="x1"] .prose')).includes('<script>'));
  ok('and did not become a script', await page.locator('[data-id="x1"] script').count() === 0);

  ok('no page errors throughout', errors.length === 0, errors.join('; '));
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
