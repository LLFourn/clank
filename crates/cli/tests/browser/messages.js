const { open, done, facts, agent } = require('./harness');
const out = [];
const ok = (name, cond, detail = '') => out.push(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? ' — ' + detail : ''}`);
(async () => {
  try {
  // ---- reply custody is visible without inventing recipient acknowledgement
  {
    const { browser, page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('codex', 'master')], {
      ledger: { plan: 'foo', gate: 'continued', dirty: null, queue: [], stash: 0, blocks: [], rows: [
        { kind: 'github', glyph: '→ ✉', text: 'message worker  Answer sent → coordinator (/external/<repo>)', age: 'now' },
        { kind: 'github', glyph: '? ✉', text: 'message coordinator  Original question', age: '1m' },
        { kind: 'github', glyph: '✓ ✉', text: 'message coordinator  Woken question', age: '1m' },
        { kind: 'github', glyph: '✓✓ ✉', text: 'message coordinator  Answered question', age: '1m' },
      ] },
    }));
    await page.click('#mode-timeline');
    const glyphs = await page.locator('#rows .glyph').allTextContents();
    ok('Timeline shows outbound custody and the mail badge',
       glyphs.length === 4 && glyphs[0] === '→ ✉' && glyphs[1] === '? ✉');
    ok('Timeline distinguishes unwoken, woken and acknowledged',
       glyphs[1] === '? ✉' && glyphs[2] === '✓ ✉' && glyphs[3] === '✓✓ ✉');
    const content = await page.textContent('#rows');
    ok('external reply names its destination without a handled tick',
       content.includes('sent → coordinator (/external/<repo>)') && !glyphs[0].includes('✓'));
    ok('event timeline text stays escaped', await page.locator('#rows repo').count() === 0);
    ok('reply timeline has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  } catch (err) {
    ok('reply timeline check completed', false, String(err));
  }
  try { await done(); } catch (err) {}
  console.log(out.join('\n'));
  process.exit(out.some((line) => line.startsWith('FAIL')) ? 1 : 0);
})();
