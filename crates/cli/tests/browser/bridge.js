// Large browser decoding and retained display, paired with the Rust
// large_harness_turns test for the real Feed/RepoSession/socket path.
const assert = require('node:assert/strict');
const { open, done, facts, agent, panes, turns, text } = require('./harness');
(async () => {
  try {
    const { browser, page, errors } = await open();
    await page.evaluate(d => window.__send('status', d), facts([agent('grok', 'master', null, null, true)]));
    await page.evaluate(d => window.__send('panes', d), panes(['grok']));
    const large = '🦀 "quote" \\\n'.repeat(100000);
    const first = text('large', 'person', large, `<p>${large}</p>`);
    await page.evaluate(d => window.__send('turns', d), turns('grok', [first]));
    await page.locator('#mode-transcript').click();
    await page.waitForFunction(n => document.querySelector('.turns').textContent.length >= n, large.length);
    assert((await page.locator('.turns').textContent()).includes(large));
    // An open socket is not proof of a current repo attachment.
    await page.evaluate(() => window.__sock.onopen());
    assert((await page.locator('#doing').textContent()).includes('reconnecting'));
    await page.evaluate(() => window.__send('connection', { connected: false }));
    assert((await page.locator('.turns').textContent()).includes(large));
    await page.evaluate(() => { window.__before = window.__sock; window.__send('connection', { connected: true }); });
    assert(!(await page.locator('#doing').textContent()).includes('reconnecting'));
    assert(await page.evaluate(() => window.__before === window.__sock));
    const second = text('large-two', 'agent', large, `<p>${large}</p>`);
    await page.evaluate(d => window.__send('turn', { agent: 'grok', session: 's1', generation: 1, turn: d }), second);
    assert.equal(await page.locator('.turn').count(), 2);
    assert.equal(errors.length, 0, errors.join('; '));
    console.log('PASS large browser frames, full display, stale/live connection notices, same-socket recovery');
    await browser.close();
  } finally { await done(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
