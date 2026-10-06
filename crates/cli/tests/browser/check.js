const { open, done, facts, agent, panes, text, turns } = require('./harness');
const out = [];
const ok = (name, cond, detail = '') => out.push(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
// Playwright treats aria-disabled as disabled for locator.click(), even
// though this tab has no native disabled attribute: it opens the reason
// why Chat is unavailable. A pointer click still uses browser hit testing,
// so covering the button or making it inert fails the following assertions.
const clickUnavailableChat = async (page) => {
  const box = await page.locator('#mode-transcript').boundingBox();
  if (!box) throw new Error('unavailable Chat tab is not visible');
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
};

(async () => {
  try {
  // ---- a finished run's end rule is a textless line in the timeline
  {
    const { page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master')], {
      ledger: { plan: null, gate: null, dirty: null, stash: 0, blocks: [], queue: [], rows: [
        { kind: 'commit', glyph: '~', sha: 'aaaaaaa', text: 'drive by', age: '1m' },
        { kind: 'run_end', text: '' },
        { kind: 'commit', glyph: '⚑', sha: 'bbbbbbb', text: 'finish', age: '2m' },
      ] },
    }));
    const rule = page.locator('#rows .row.run_end');
    ok('the run end draws one line with no text',
       await rule.count() === 1 && (await rule.textContent()).trim() === '');
    ok('no page errors (run end)', errors.length === 0, errors.join('; '));
  }

  // ---- drafts collapse to one read-only row under the queue, absent at zero
  {
    const { page, errors } = await open();
    const frame = (drafts) => facts([agent('claude', 'master')], {
      ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, stash: 0, blocks: [], rows: [], drafts,
        queue: [{ name: 'queued-idea', priority: 500, handle: '500-queued-idea', page: 'queue/500-queued-idea.html' }] },
    });
    await page.evaluate((d) => window.__send('status', d), frame(2));
    const text = await page.textContent('#queue');
    ok('the drafts count shows under the queue',
       text.includes('drafts: 2') && text.indexOf('queued-idea') < text.indexOf('drafts: 2'));
    ok('the drafts row offers no action', await page.locator('#queue .drafts a, #queue .drafts button').count() === 0);
    await page.evaluate((d) => window.__send('status', d), frame(0));
    ok('no drafts, no row', !(await page.textContent('#queue')).includes('drafts'));
    ok('no page errors (drafts)', errors.length === 0, errors.join('; '));
  }

  // ---- an agent's pending question shows in the header and the picker,
  // to a page that connects after it was asked, and goes when it ends
  {
    const { page, errors } = await open();
    const frame = (asking) => facts([{ ...agent('claude', 'master', null, 'reviewed abc'), asking }]);
    await page.evaluate((d) => window.__send('status', d), frame('Direction <b>'));
    ok('a late viewer sees what the agent is asking',
       (await page.textContent('#doing')).includes('asking you “Direction <b>”'));
    await page.click('#who');
    ok('the picker says it too',
       (await page.textContent('.picker .opt[data-key="pin:claude"]')).includes('asking you'));
    await page.keyboard.press('Escape');
    await page.evaluate((d) => window.__send('status', d), frame(null));
    ok('an answered question leaves the header',
       !(await page.textContent('#doing')).includes('asking') && (await page.textContent('#doing')).includes('reviewed abc'));
    ok('no page errors (asking)', errors.length === 0, errors.join('; '));
  }

  // ---- blocks can be answered in the page without losing a draft on status refresh
  {
    const { browser, page, errors } = await open();
    await page.evaluate(() => {
      window.__rejectBlock = true;
      window.fetch = async (url, opts) => {
        window.__frames.push([url, opts && opts.body]);
        if (url === '/blocks/answer' && window.__rejectBlock)
          return { ok: false, status: 409, json: async () => ({ why: 'the question changed' }) };
        return { ok: true, status: 204 };
      };
    });
    const frame = () => facts([agent('claude', 'reviewer')], {
      ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: [], stash: 0,
        blocks: [{ agent: 'claude', name: 'which-option', question: 'Which option?\nExplain why.' }], rows: [] },
    });
    await page.evaluate((d) => window.__send('status', d), frame());
    const block = page.locator('#strip form.block');
    ok('pending block shows agent, name and full question',
       await block.count() === 1 && (await block.textContent()).includes('claude · which-option')
       && (await block.textContent()).includes('Which option?\nExplain why.'));
    await page.fill('#strip textarea', 'Use the first option.');
    await page.evaluate((d) => window.__send('status', d), frame());
    ok('unchanged status preserves the answer draft', await page.inputValue('#strip textarea') === 'Use the first option.');
    await page.click('#strip button[type=submit]');
    await page.waitForFunction(() => document.querySelector('#strip .block-error').textContent.includes('question changed'));
    ok('refused answer explains why and stays editable', await page.inputValue('#strip textarea') === 'Use the first option.'
       && await page.locator('#strip form.block').count() === 1);
    await page.evaluate(() => { window.__rejectBlock = false; });
    await page.click('#strip button[type=submit]');
    await page.waitForFunction(() => document.querySelector('#strip').hidden);
    const sends = await page.evaluate(() => window.__frames.filter((f) => f[0] === '/blocks/answer').map((f) => JSON.parse(f[1])));
    ok('answer carries block identity and question; success clears pending block', sends.length === 2
       && sends[1].agent === 'claude' && sends[1].name === 'which-option'
       && sends[1].question === 'Which option?\nExplain why.' && sends[1].answer === 'Use the first option.');
    ok('block answer has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- Timeline is a peer of Chat and terminal on a phone
  {
    const { browser, page, errors } = await open();
    const button = page.locator('#mode-timeline');
    ok('Timeline sits beside Chat and terminal',
       await button.textContent() === 'Timeline' && await button.getAttribute('aria-controls') === 'ledger'
       && await page.locator('.state .modes > button').count() === 3
       && await page.locator('.bar > #more').count() === 0);
    await button.click();
    const layout = await page.evaluate(() => {
      const ledger = document.querySelector('#ledger').getBoundingClientRect();
      const bar = document.querySelector('.bar').getBoundingClientRect();
      return { top: ledger.top, barBottom: bar.bottom, width: ledger.width,
        position: getComputedStyle(document.querySelector('#ledger')).position };
    });
    ok('Timeline uses the main content area without covering the header',
       await page.isVisible('#ledger') && await page.isHidden('.agent') && await page.isHidden('#say')
       && await button.getAttribute('aria-pressed') === 'true'
       && layout.position !== 'fixed' && layout.top >= layout.barBottom && layout.width > 300);
    await page.evaluate((d) => window.__send('status', d), facts([agent('codex', 'master')]));
    ok('a status update leaves Timeline selected', await page.isVisible('#ledger')
       && await button.getAttribute('aria-pressed') === 'true');
    await page.click('#mode-terminal');
    ok('terminal tab restores the agent view and composer',
       await page.isHidden('#ledger') && await page.isVisible('.agent') && await page.isVisible('#say')
       && await button.getAttribute('aria-pressed') === 'false');
    await button.focus();
    await page.keyboard.press('Enter');
    ok('Enter selects Timeline', await button.getAttribute('aria-pressed') === 'true');
    await page.keyboard.press('Space');
    ok('Space keeps the selected tab open', await button.getAttribute('aria-pressed') === 'true');
    await page.setViewportSize({ width: 1100, height: 780 });
    await page.waitForFunction(() => !document.body.classList.contains('timeline-active'));
    ok('wide screen keeps the ledger as a sidebar and hides its tab',
       await page.isVisible('#ledger') && await page.isVisible('.agent') && await page.isHidden('#mode-timeline'));
    await page.setViewportSize({ width: 390, height: 780 });
    ok('returning to phone size restores the agent mode',
       await page.isHidden('#ledger') && await page.isVisible('.agent'));
    ok('timeline tabs have no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- generated plan pages stay inside the agent website
  // ---- Timeline contains navigation only, at phone and desktop widths
  {
    const { browser, page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('codex', 'master')], {
      ledger: { plan: 'foo', gate: 'continued', dirty: null,
        queue: [{ handle: '500-foo', name: 'foo', priority: 500, page: 'queue/500-foo.html' }],
        stash: 1, stash_entries: [{ stem: 'bar', commits: 2, ready: true, page: 'stash/bar.html' }],
        blocks: [], rows: [{ kind: 'header', text: 'foo', page: 'plan/foo.html' }],
      },
    }));
    const boundary = () => page.evaluate(() => {
      const ledger = document.querySelector('#ledger');
      const forbidden = 'form,input,textarea,select,details,[contenteditable]:not([contenteditable="false"]),[onclick],[onchange],[onsubmit],.queue-editor,.stash-detail,.team,.management-page';
      const buttons = [...ledger.querySelectorAll('button')].filter((b) => b.dataset.navOnly !== 'true');
      const links = [...ledger.querySelectorAll('a[href]')].filter((a) => {
        const url = new URL(a.href);
        return url.origin !== location.origin || !(url.pathname.startsWith('/html/')
          || ['#new-queued-plan', '#manage-team', '#device-settings'].includes(url.hash));
      });
      return { forbidden: ledger.querySelectorAll(forbidden).length, buttons: buttons.length, links: links.length };
    });
    await page.click('#mode-timeline');
    const phone = await boundary();
    ok('phone Timeline contains only summary and navigation',
       phone.forbidden === 0 && phone.buttons === 0 && phone.links === 0, JSON.stringify(phone));
    await page.click('#manage-team');
    ok('team link opens a full management page', await page.isVisible('#team-page')
       && await page.isHidden('#ledger') && await page.isHidden('#plan-frame'));
    await page.click('#plan-back');
    await page.click('#device-settings');
    ok('device link opens settings in the main area', await page.isVisible('#device-page')
       && await page.isHidden('#ledger') && await page.isVisible('#forget'));
    await page.evaluate(() => {
      window.fetch = async (url, opts) => {
        window.__frames.push([url, opts?.body]);
        return { ok: url !== '/forget', status: url === '/forget' ? 409 : 204 };
      };
    });
    await page.click('#forget');
    await page.waitForFunction(() => document.querySelector('#forget').textContent.includes('could not sign out'));
    ok('device sign-out refusal stays on its settings page', await page.isVisible('#device-page')
       && await page.isEnabled('#forget')
       && await page.evaluate(() => window.__frames.some((f) => f[0] === '/forget')));
    await page.click('#plan-back');
    await page.click('#queue-new');
    ok('new-plan link opens its editor in the main area', await page.isVisible('#queue-editor')
       && await page.isHidden('#ledger'));
    await page.click('#plan-back');
    await page.setViewportSize({ width: 1100, height: 780 });
    const wide = await boundary();
    ok('desktop Timeline contains only summary and navigation',
       wide.forbidden === 0 && wide.buttons === 0 && wide.links === 0, JSON.stringify(wide));
    await page.click('#manage-team');
    ok('desktop management uses the main area beside Timeline', await page.isVisible('#team-page')
       && await page.isVisible('#ledger')
       && await page.locator('#team-page').evaluate((el) => el.getBoundingClientRect().width > 500));
    ok('Timeline boundary has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- generated plan pages stay inside the agent website
  {
    const { browser, page, errors } = await open();
    const top = page.url();
    await page.route('**/html/**', (route) => {
      const path = new URL(route.request().url()).pathname;
      const body = path === '/html/style.css' ? 'h1 { color: rgb(18, 52, 86); }'
        : path === '/html/plan/foo.html' ? '<!doctype html><link rel="stylesheet" href="../style.css"><main><h1>Plan foo</h1><p>Readable <strong>milestone</strong></p><a href="../commit/abc123.html">Open commit</a></main>'
        : path === '/html/commit/abc123.html' ? '<!doctype html><link rel="stylesheet" href="../style.css"><main><h1>Commit abc123</h1><a href="../plan/foo.html">Back to plan</a></main>'
        : '<h1>Missing page</h1>';
      route.fulfill({ status: path === '/html/style.css' || path === '/html/plan/foo.html' || path === '/html/commit/abc123.html' ? 200 : 404,
        contentType: path.endsWith('.css') ? 'text/css' : 'text/html', body });
    });
    await page.evaluate((d) => window.__send('status', d), facts([agent('codex', 'master')], {
      ledger: { plan: 'foo', gate: 'continued', dirty: null, queue: [], stash: 0, blocks: [],
        rows: [{ kind: 'header', text: 'foo', page: 'plan/foo.html' }] },
    }));
    await page.click('#mode-timeline');
    await page.click('#rows .row.header a');
    const embedded = page.frameLocator('#plan-frame');
    await embedded.locator('h1').waitFor();
    ok('selecting a plan keeps the agent chrome and top-level URL', page.url() === top
       && await page.isVisible('#who') && await page.isVisible('#mode-timeline')
       && await page.isVisible('#plan-view') && await page.isHidden('#views')
       && await page.locator('#plan-frame').evaluate((el) => el.getBoundingClientRect().height > 200));
    ok('the plan keeps its prose and generated stylesheet',
       await embedded.locator('h1').textContent() === 'Plan foo'
       && (await embedded.locator('p').textContent()).includes('Readable milestone')
       && await embedded.locator('h1').evaluate((el) => getComputedStyle(el).color) === 'rgb(18, 52, 86)');
    await embedded.locator('a').click();
    await embedded.locator('h1').filter({ hasText: 'Commit abc123' }).waitFor();
    ok('plan links navigate inside the viewer', page.url() === top
       && await embedded.locator('h1').textContent() === 'Commit abc123'
       && await page.isVisible('#plan-back'));
    await page.click('#plan-back');
    ok('Back returns to the Timeline tab', await page.isVisible('#ledger')
       && await page.isHidden('.agent') && await page.getAttribute('#mode-timeline', 'aria-pressed') === 'true');
    await page.click('#manage-team');
    ok('the return path reaches team management outside Timeline', page.url() === top
       && await page.isVisible('#plan-view') && await page.isHidden('#ledger')
       && await page.isVisible('#team-page') && await page.isVisible('#team-rows'));
    ok('embedded plan has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- queue and stash names open their exact generated pages
  {
    const { browser, page, errors } = await open();
    const pages = {
      '/html/queue/100-same.html': '<main><h1>First queued copy</h1><p>Its full rendered body is readable.</p></main>',
      '/html/queue/900-same.html': '<main><h1>Second queued copy</h1><p>The other file has its own page.</p></main>',
      '/html/stash/same.html': '<main><h1>Stashed copy</h1><p>Two commits and a dependency.</p></main>',
    };
    await page.route('**/html/**', (route) => {
      const path = new URL(route.request().url()).pathname;
      route.fulfill({ status: pages[path] ? 200 : 404, contentType: 'text/html', body: pages[path] || 'Missing page' });
    });
    const frame = (rows) => facts([], { ledger: {
      plan: null, gate: null, dirty: null,
      queue: [
        { handle: '100-same', name: 'same', priority: 100, page: 'queue/100-same.html' },
        { handle: '900-same', name: 'same', priority: 900, page: 'queue/900-same.html' },
      ],
      stash: 1, stash_entries: [{ stem: 'same', commits: 2, waiting_for: 'other', ready: false, page: 'stash/same.html' }],
      blocks: [], rows,
    } });
    const rows = Array.from({ length: 65 }, (_, i) => ({ kind: 'notice', text: `Ledger row ${i}`, age: '' }));
    await page.evaluate((d) => window.__send('status', d), frame(rows));
    await page.click('#mode-timeline');
    ok('equal queue names carry distinct generated-page targets',
       await page.getAttribute('#queue .q[data-handle="100-same"] a.nm', 'href') === 'html/queue/100-same.html'
       && await page.getAttribute('#queue .q[data-handle="900-same"] a.nm', 'href') === 'html/queue/900-same.html');
    await page.evaluate(() => { document.querySelector('#ledger').scrollTop = 70; });
    const before = await page.evaluate(() => document.querySelector('#ledger').scrollTop);
    await page.click('#queue .q[data-handle="100-same"] a.nm');
    const embedded = page.frameLocator('#plan-frame');
    await embedded.locator('h1').waitFor();
    ok('queued name opens full rendered Markdown outside Timeline',
       await embedded.locator('h1').textContent() === 'First queued copy'
       && (await embedded.locator('p').textContent()).includes('full rendered body')
       && await page.isVisible('#plan-view') && await page.isHidden('#ledger')
       && await page.textContent('#plan-back') === '← Timeline');
    await page.evaluate((d) => window.__send('status', d), frame(rows.slice(0, 64)));
    ok('a status frame keeps the exact selected queue page',
       (await page.getAttribute('#plan-frame', 'src')).endsWith('/html/queue/100-same.html')
       && await embedded.locator('h1').textContent() === 'First queued copy');
    await page.click('#plan-back');
    const after = await page.evaluate(() => document.querySelector('#ledger').scrollTop);
    ok('Back returns to Timeline at its scroll position',
       await page.isVisible('#ledger') && await page.isHidden('#plan-view')
       && await page.getAttribute('#mode-timeline', 'aria-pressed') === 'true' && Math.abs(after - before) <= 2);
    await page.click('#queue .q[data-handle="900-same"] a.open');
    await embedded.locator('h1').filter({ hasText: 'Second queued copy' }).waitFor();
    ok('Open plan keeps same-name queue entries separate', await embedded.locator('h1').textContent() === 'Second queued copy');
    await page.click('#plan-back');
    await page.setViewportSize({ width: 1100, height: 780 });
    await page.click('#stash-list .item[data-stem="same"] a.name');
    await embedded.locator('h1').filter({ hasText: 'Stashed copy' }).waitFor();
    ok('wide stashed page occupies main area with Timeline beside it',
       await page.isVisible('#plan-view') && await page.isVisible('#ledger')
       && await embedded.locator('h1').textContent() === 'Stashed copy'
       && await page.textContent('#plan-back') === '← Agents');
    await page.evaluate(() => { document.querySelector('#ledger').scrollTop = 180; });
    const wideScroll = await page.evaluate(() => document.querySelector('#ledger').scrollTop);
    await page.click('#plan-back');
    const afterWide = await page.evaluate(() => document.querySelector('#ledger').scrollTop);
    ok('wide Back keeps the sidebar scroll and focuses its entry',
       await page.isHidden('#plan-view')
       && await page.evaluate(() => document.activeElement?.matches('#stash-list .item[data-stem="same"] a.name'))
       && wideScroll > 0 && Math.abs(afterWide - wideScroll) <= 2);
    ok('queued and stashed navigation has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- queued Markdown can be created, opened, edited and reordered
  {
    const { browser, page, errors } = await open();
    await page.route('**/html/queue/**', (route) => route.fulfill({ status: 200, contentType: 'text/html', body: '<main><h1>later</h1><p>Rendered queued plan</p></main>' }));
    await page.evaluate(() => {
      window.fetch = async (url, opts) => {
        const ask = JSON.parse(opts.body);
        window.__frames.push([url, opts.body]);
        if (ask.do === 'preview') return { ok: true, status: 200, json: async () => ({ html: '<h1>Preview</h1><p>Edited Markdown</p>' }) };
        if (ask.do === 'open') return { ok: true, status: 200, json: async () => ({
          handle: ask.handle, name: 'later', priority: 500, body: '# later\nFirst draft\n', revision: 'r1',
        }) };
        if (ask.do === 'save' && ask.body.includes('refused'))
          return { ok: false, status: 409, json: async () => ({ why: 'changed since it was opened' }) };
        if (ask.do === 'add') return { ok: true, status: 200, json: async () => ({ handle: '500-later', page: 'queue/500-later.html' }) };
        if (ask.do === 'priority') return { ok: true, status: 200, json: async () => ({ handle: '100-later', page: 'queue/100-later.html' }) };
        return { ok: true, status: 204 };
      };
    });
    const frame = (queue) => facts([], { ledger: {
      plan: 'foo', gate: 'unreviewed', dirty: null, queue, stash: 0, blocks: [], rows: [],
    } });
    await page.evaluate((d) => window.__send('status', d), frame([]));
    await page.click('#mode-timeline');
    await page.click('#queue-new');
    ok('new plan editor occupies the main area outside Timeline', await page.isVisible('#queue-editor')
       && await page.isHidden('#ledger') && await page.locator('#ledger textarea, #ledger form#queue-edit-form').count() === 0);
    await page.fill('#queue-name', 'later');
    await page.fill('#queue-priority', '500');
    await page.fill('#queue-body', '# later\nFirst draft\n');
    await page.waitForFunction(() => document.querySelector('#queue-preview').textContent.includes('Edited Markdown'));
    ok('new plan has a rendered preview beside its source', await page.isVisible('#queue-preview'));
    await page.click('#queue-edit-form button[type=submit]');
    const asks = () => page.evaluate(() => window.__frames.filter((f) => f[0] === '/queue').map((f) => JSON.parse(f[1])));
    await page.waitForFunction(() => document.querySelector('#queue-editor').hidden);
    ok('new queued plan sends its Markdown and priority', (await asks()).some((a) =>
       a.do === 'add' && a.name === 'later' && a.priority === 500 && a.body === '# later\nFirst draft\n'));
    await page.evaluate((d) => window.__send('status', d), frame([{ handle: '500-later', name: 'later', priority: 500, page: 'queue/500-later.html' }]));
    ok('creation opens the newly rendered page when it appears', await page.isVisible('#plan-frame')
       && await page.isVisible('#queue-page-tools')
       && (await page.locator('#plan-frame').getAttribute('src')).includes('500-later.html'));
    await page.click('#plan-back');
    ok('Timeline queue row contains navigation and read-only priority',
       await page.locator('#queue .q button, #queue .q input, #queue .q form').count() === 0
       && await page.locator('#queue .q a[href]').count() === 2);
    await page.click('#queue .q[data-handle="500-later"] a.nm');
    await page.click('#queue-open-edit');
    ok('edit opens the queued Markdown source', await page.inputValue('#queue-body') === '# later\nFirst draft\n');
    await page.fill('#queue-body', '# later\nRevised\n');
    await page.evaluate((d) => window.__send('status', d), frame([{ handle: '500-later', name: 'later', priority: 500, page: 'queue/500-later.html' }]));
    ok('a status frame preserves the page draft and focus', await page.inputValue('#queue-body') === '# later\nRevised\n'
       && await page.evaluate(() => document.activeElement?.id === 'queue-body') && await page.isVisible('#queue-editor'));
    await page.click('#queue-edit-form button[type=submit]');
    await page.waitForFunction(() => document.querySelector('#queue-editor').hidden);
    ok('save carries the original source for stale edit refusal', (await asks()).some((a) =>
       a.do === 'save' && a.handle === '500-later' && a.original === '# later\nFirst draft\n' && a.revision === 'r1'
       && a.body === '# later\nRevised\n'));
    await page.fill('#queue-page-priority', '100');
    await page.click('#queue-set-priority');
    ok('priority change addresses the same file', (await asks()).some((a) =>
       a.do === 'priority' && a.handle === '500-later' && a.priority === 100));
    await page.evaluate((d) => window.__send('status', d), frame([
      { handle: '300-later', name: 'later', priority: 300, page: 'queue/300-later.html' },
    ]));
    ok('missing priority target reports an unknown outcome instead of waiting forever',
       await page.isVisible('#queue-result')
       && (await page.textContent('#qwhy')).includes('absent from the latest status')
       && await page.isHidden('#queue-page-tools'));
    await page.evaluate((d) => window.__send('status', d), frame([
      { handle: '050-urgent', name: 'urgent', priority: 50, page: 'queue/050-urgent.html' },
      { handle: '100-later', name: 'later', priority: 100, page: 'queue/100-later.html' },
    ]));
    ok('queue redraw shows the new order', await page.locator('#queue .q .nm').allTextContents()
       .then((names) => names.join(',') === 'urgent,later') && await page.isVisible('#queue-page-tools')
       && (await page.locator('#plan-frame').getAttribute('src')).includes('100-later.html'));
    await page.click('#queue-open-edit');
    ok('edit after priority change targets the moved handle', (await asks()).some((a) =>
       a.do === 'open' && a.handle === '100-later'));
    await page.fill('#queue-body', '# later\nrefused\n');
    await page.click('#queue-edit-form button[type=submit]');
    await page.waitForFunction(() => document.querySelector('#qwhy').textContent.includes('changed since'));
    ok('a failed save explains itself and keeps the editor', await page.isVisible('#queue-editor')
       && (await page.textContent('#qwhy')).includes('changed since it was opened'));
    await page.click('#queue-cancel');
    ok('Cancel returns to the rendered plan without saving the draft', await page.isVisible('#plan-frame')
       && await page.isHidden('#queue-editor') && await page.isVisible('#queue-page-tools'));
    ok('queued editor has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- page actions use exact queue handles and stay on the owning page
  {
    const { browser, page, errors } = await open();
    await page.route('**/html/queue/**', (route) => route.fulfill({ status: 200, contentType: 'text/html', body: '<main><h1>Queued plan</h1></main>' }));
    await page.evaluate(() => {
      window.fetch = async (url, opts) => {
        const ask = JSON.parse(opts.body);
        window.__frames.push([url, opts.body]);
        if (ask.do === 'open') return { ok: true, status: 200, json: async () => ({
          handle: ask.handle, name: 'same', priority: ask.handle.startsWith('100') ? 100 : 200,
          body: '# same\n' + 'A long section.\n'.repeat(120), revision: 'r1',
        }) };
        if (ask.do === 'preview') return { ok: true, status: 200, json: async () => ({ html: '<h1>same</h1>' }) };
        if (ask.do === 'promote' && window.__refusePromote) return { ok: false, status: 409, json: async () => ({
          why: 'ambiguous queue item; candidates: 100-same.md, 200-same.md',
        }) };
        return { ok: true, status: 204 };
      };
    });
    const entries = [
      { handle: '100-same', name: 'same', priority: 100, page: 'queue/100-same.html' },
      { handle: '200-same', name: 'same', priority: 200, page: 'queue/200-same.html' },
    ];
    const frame = (queue) => facts([], { ledger: {
      plan: 'foo', gate: 'unreviewed', dirty: null, queue, stash: 0, blocks: [], rows: [],
    } });
    const asks = () => page.evaluate(() => window.__frames.filter((f) => f[0] === '/queue').map((f) => JSON.parse(f[1])));
    await page.evaluate((d) => window.__send('status', d), frame(entries));
    await page.click('#mode-timeline');
    await page.click('#queue .q[data-handle="100-same"] a.nm');
    await page.click('#queue-open-edit');
    const phone = await page.evaluate(() => ({
      editor: document.querySelector('#queue-editor').getBoundingClientRect().width,
      page: document.querySelector('#plan-view').getBoundingClientRect().width,
      scroll: document.querySelector('#queue-editor').scrollHeight > document.querySelector('#queue-editor').clientHeight,
    }));
    ok('phone editor uses the page width and scrolls long Markdown', phone.editor > 300
       && Math.abs(phone.editor - phone.page) < 2 && phone.scroll);
    await page.fill('#queue-body', '# same\nUnsaved change\n');
    await page.evaluate((d) => window.__send('status', d), frame(entries));
    ok('same-name status refresh keeps the exact draft and focus', await page.inputValue('#queue-body') === '# same\nUnsaved change\n'
       && await page.evaluate(() => document.activeElement?.id === 'queue-body'));
    await page.setViewportSize({ width: 1100, height: 780 });
    const wide = await page.evaluate(() => {
      const main = document.querySelector('#queue-editor .editor-main').getBoundingClientRect();
      const source = document.querySelector('#queue-body').getBoundingClientRect();
      const preview = document.querySelector('#queue-preview').parentElement.getBoundingClientRect();
      return { main: main.width, source: source.width, preview: preview.width, sourceX: source.x, previewX: preview.x };
    });
    ok('wide editor has readable source and preview columns', wide.main > 500 && wide.source > 220
       && wide.preview > 220 && wide.previewX > wide.sourceX, JSON.stringify(wide));
    await page.click('#queue-cancel');
    await page.evaluate(() => { window.__refusePromote = true; });
    await page.click('#queue-promote');
    ok('promotion asks for confirmation on the plan page', await page.textContent('#queue-promote') === 'Promote?'
       && !(await asks()).some((a) => a.do === 'promote'));
    await page.click('#queue-promote');
    ok('ambiguous refusal stays on the selected page and names both stems', await page.isVisible('#plan-frame')
       && (await page.textContent('#qwhy')).includes('100-same.md, 200-same.md')
       && (await asks()).filter((a) => a.do === 'promote').every((a) => a.handle === '100-same'));
    await page.evaluate((d) => window.__send('status', d), frame([entries[1]]));
    ok('lost exact handle becomes stale without selecting its namesake', await page.isDisabled('#queue-promote')
       && (await page.textContent('#qwhy')).includes('moved or left'));
    await page.click('#plan-back');
    await page.click('#queue .q[data-handle="200-same"] a.nm');
    await page.click('#queue-unqueue');
    await page.click('#queue-unqueue');
    ok('unqueue acts on the second exact handle and shows a result page', await page.isVisible('#queue-result')
       && await page.isHidden('#queue-page-tools') && (await asks()).some((a) => a.do === 'unqueue' && a.handle === '200-same'));
    await page.click('#plan-back');
    await page.evaluate(() => { window.__refusePromote = false; });
    await page.evaluate((d) => window.__send('status', d), frame(entries));
    await page.click('#queue .q[data-handle="100-same"] a.nm');
    await page.click('#queue-promote');
    await page.click('#queue-promote');
    ok('successful promotion has a result and cannot be resubmitted', await page.isVisible('#queue-result')
       && await page.isHidden('#queue-page-tools')
       && (await asks()).filter((a) => a.do === 'promote').at(-1).handle === '100-same');
    ok('exact-handle page actions have no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- stashed plans expose their source and require a confirmed restore
  {
    const { browser, page, errors } = await open();
    await page.route('**/html/stash/**', (route) => route.fulfill({ status: 200, contentType: 'text/html',
      body: `<main><h1>Stashed plan</h1><p>waiting for dependency</p><h2>Stashed commits</h2><ul><li>first</li><li>second</li></ul><article>${'<p>the stashed plan</p>'.repeat(100)}</article></main>`,
    }));
    await page.evaluate(() => {
      window.__rejectStash = true;
      window.fetch = async (url, opts) => {
        const ask = JSON.parse(opts.body);
        window.__frames.push([url, opts.body]);
        if (url === '/stash' && ask.do === 'open') return {
          ok: true, status: 200, json: async () => ({ stem: ask.stem, revision: 'stash-r1',
            body: `# ${ask.stem}\n\nthe stashed plan\n`, commits: 2, waiting_for: 'dependency' }),
        };
        if (url === '/stash' && window.__rejectStash) return {
          ok: false, status: 409, json: async () => ({ why: 'stashed plan changed since it was opened' }),
        };
        return { ok: true, status: 204 };
      };
    });
    const frame = (stash_entries) => facts([], { ledger: {
      plan: null, gate: null, dirty: null, queue: [], stash: stash_entries.length,
      stash_entries, blocks: [], rows: [],
    } });
    await page.evaluate((d) => window.__send('status', d), frame([
      { stem: 'foo', commits: 2, waiting_for: 'dependency', ready: false, page: 'stash/foo.html' },
      { stem: 'other', commits: 1, waiting_for: null, ready: true, page: 'stash/other.html' },
    ]));
    await page.click('#mode-timeline');
    ok('the stash list shows both plans and readiness',
       (await page.locator('#stash-list .item').count()) === 2
       && (await page.textContent('#stash-list')).includes('ready'));
    ok('Timeline stash rows contain navigation without content or action controls',
       await page.locator('#stash-list button, #stash-list form, #stash-list pre, #stash-detail').count() === 0
       && await page.locator('#stash-list .item[data-stem="foo"] a[href]').count() === 2);
    await page.click('#stash-list .item[data-stem="foo"] a.name');
    const embedded = page.frameLocator('#plan-frame');
    await embedded.locator('article').waitFor();
    await page.waitForFunction(() => !document.querySelector('#stash-restore').disabled);
    ok('stash page shows rendered body, dependency and commit history',
       await page.isVisible('#plan-view') && await page.isHidden('#ledger')
       && (await embedded.locator('article').textContent()).includes('the stashed plan')
       && (await embedded.locator('ul').textContent()).includes('second')
       && (await page.textContent('#stash-page-state')).includes('dependency'));
    const phone = await page.evaluate(() => ({
      page: document.querySelector('#plan-view').getBoundingClientRect().width,
      frame: document.querySelector('#plan-frame').getBoundingClientRect().width,
    }));
    ok('long stash page uses the phone content width', phone.page > 300 && Math.abs(phone.page - phone.frame) < 2);
    await page.setViewportSize({ width: 1100, height: 780 });
    ok('stash detail occupies the wide main area', await page.isVisible('#plan-view') && await page.isVisible('#ledger'));
    const restores = () => page.evaluate(() => window.__frames.filter((f) =>
      f[0] === '/stash' && JSON.parse(f[1]).do === 'restore').map((f) => JSON.parse(f[1])));
    await page.click('#stash-restore');
    ok('restore confirmation belongs to the page', (await restores()).length === 0
       && await page.textContent('#stash-restore') === 'Restore?');
    await page.click('#stash-restore');
    await page.waitForFunction(() => document.querySelector('#stash-why').textContent.includes('changed since'));
    ok('a refused restore stays on the page and requires a fresh revision', await page.isVisible('#plan-frame')
       && (await page.textContent('#stash-why')).includes('changed since')
       && await page.isDisabled('#stash-restore') && await page.isEnabled('#stash-refresh'));
    ok('restore names the opened stash revision', (await restores()).some((a) =>
       a.stem === 'foo' && a.revision === 'stash-r1'));
    await page.evaluate(() => { window.__rejectStash = false; });
    await page.click('#stash-refresh');
    await page.waitForFunction(() => !document.querySelector('#stash-restore').disabled);
    await page.click('#stash-restore');
    await page.click('#stash-restore');
    await page.waitForFunction(() => !document.querySelector('#stash-result').hidden);
    ok('successful restore leaves a result page without a stale action', await page.isVisible('#stash-result')
       && await page.isHidden('#stash-page-tools') && await page.isHidden('#plan-frame'));
    await page.evaluate((d) => window.__send('status', d), frame([
      { stem: 'other', commits: 1, waiting_for: null, ready: true, page: 'stash/other.html' },
    ]));
    await page.click('#plan-back');
    ok('restoring one stash leaves the other visible',
       await page.locator('#stash-list .item').count() === 1
       && (await page.textContent('#stash-list')).includes('other'));
    await page.click('#stash-list .item[data-stem="other"] a.name');
    await page.evaluate((d) => window.__send('status', d), frame([]));
    ok('a removed stash is stale without retargeting Restore', await page.isDisabled('#stash-restore')
       && (await page.textContent('#stash-why')).includes('restored elsewhere')
       && (await restores()).every((a) => a.stem === 'foo'));
    ok('stash controls have no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- Chat explains unavailable states and opens a Codex conversation
  {
    const { browser, page, errors } = await open();
    ok('the mode is named Chat', await page.textContent('#mode-transcript') === 'Chat');
    await page.click('#mode-transcript');
    ok('before status, Chat explains why it cannot open',
       await page.isVisible('#chat-reason')
       && (await page.textContent('#chat-reason')).includes('Waiting for this session'));
    await page.click('#chat-reason-close');
    await page.evaluate((d) => window.__send('status', d), facts([]));
    await clickUnavailableChat(page);
    ok('without an agent, Chat explains why it cannot open',
       await page.isVisible('#chat-reason')
       && (await page.textContent('#chat-reason')).includes('No agent is selected'));
    await page.click('#chat-reason-close');

    await page.evaluate((d) => window.__send('status', d), facts([
      { ...agent('codex', 'master', null, null, true), tool: 'codex', session: 's1' },
    ]));
    await page.evaluate((d) => window.__send('panes', d), panes(['codex']));
    await page.evaluate((d) => window.__send('turns', d), turns('codex', [
      text('c1', 'person', 'Codex fixture question', 'Codex fixture question'),
      text('c2', 'agent', 'Codex fixture answer', 'Codex fixture answer'),
    ]));
    ok('Codex Chat opens when history exists', await page.isVisible('.view.transcript.on')
       && (await page.textContent('.view.transcript.on')).includes('Codex fixture answer'));

    const reasons = [
      'This agent has no session binding.',
      'This agent has no bound session.',
      "This agent's tool does not provide Chat history.",
      'Clank has no home directory to find Chat history.',
      'No Chat history file was found for this bound session.',
      "Clank could not read this agent's session binding.",
      "This agent's label is invalid.",
    ];
    for (const reason of reasons) {
      await page.evaluate((d) => window.__send('status', d), facts([
        { ...agent('codex', 'master'), tool: 'codex', session: 's1', chat_unavailable_reason: reason },
      ]));
      ok('unavailable Chat opens its explanation', await page.getAttribute('#mode-transcript', 'disabled') === null
         && await page.getAttribute('#mode-transcript', 'aria-disabled') === 'true');
      await clickUnavailableChat(page);
      ok(`Chat explains ${reason}`, await page.isVisible('#chat-reason')
         && await page.textContent('#chat-reason-text') === reason);
      await page.click('#chat-reason-close');
    }
    await page.fill('#text', 'hold Codex');
    await page.evaluate((d) => window.__send('status', d), facts([]));
    await clickUnavailableChat(page);
    ok('a departed agent gets its own Chat reason',
       await page.isVisible('#chat-reason')
       && (await page.textContent('#chat-reason')).includes('codex has left the roster'));
    ok('Chat availability has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- repo team management uses the same roster actions as the TUI
  {
    const { browser, page, errors } = await open();
    await page.evaluate(() => {
      const original = window.fetch;
      window.fetch = (url, opts) => url === '/agents/candidates'
        ? Promise.resolve({ ok: true, json: async () => [
            { label: 'replacement', tool: 'codex' }, { label: 'spare', tool: 'grok' },
          ] })
        : original(url, opts);
    });
    const team = (role = 'commit') => facts([
      { ...agent('claude', 'master'), tool: 'claude', invocation: 'claude --safe', session: 'session-one' },
      { ...agent('grok', role), tool: 'grok', invocation: 'grok', session: null },
    ]);
    await page.evaluate((d) => window.__send('status', d), team());
    await page.click('#mode-timeline');
    await page.click('#manage-team');
    await page.waitForFunction(() => document.querySelector('#team-library option')?.value === 'replacement');
    await page.click('.team .member[data-label="claude"] > summary');
    ok('agent configuration shows invocation and binding',
       (await page.textContent('.team .member[data-label="claude"]')).includes('claude --safe')
       && (await page.textContent('.team .member[data-label="claude"]')).includes('session-one'));

    await page.fill('#team-add-name', 'codex');
    await page.selectOption('#team-add-tool', 'codex');
    await page.selectOption('#team-add-review', 'gate');
    await page.focus('#team-add-name');
    await page.evaluate((d) => window.__send('status', d), facts([
      ...team().facts.agents,
      { ...agent('observer', 'standby'), tool: 'codex', invocation: 'codex', session: null },
    ]));
    ok('roster status preserves the management page draft and focus', await page.isVisible('#team-page')
       && await page.inputValue('#team-add-name') === 'codex'
       && await page.evaluate(() => document.activeElement?.id === 'team-add-name'));
    await page.click('#team-add button');
    const actions = () => page.evaluate(() => window.__frames.filter((f) => f[0] === '/agents').map((f) => JSON.parse(f[1])));
    ok('adding an inline agent sends its tool and tier',
       (await actions()).some((a) => a.do === 'add' && a.label === 'codex' && a.tool === 'codex' && a.review === 'gate'));
    await page.fill('#team-add-name', 'spare');
    await page.selectOption('#team-add-tool', '');
    await page.selectOption('#team-add-review', 'plan');
    await page.click('#team-add button');
    ok('adding a library agent keeps its saved configuration',
       (await actions()).some((a) => a.do === 'add' && a.label === 'spare' && a.tool === null && a.review === 'plan'));
    await page.evaluate((d) => window.__send('status', d), facts([
      ...team().facts.agents,
      { ...agent('codex', 'gate'), tool: 'codex', invocation: 'codex', session: null },
    ]));
    ok('a roster update shows the added agent', await page.locator('.team .member[data-label="codex"]').count() === 1);

    await page.click('.team .member[data-label="grok"] > summary');
    await page.selectOption('.team .member[data-label="grok"] select[data-do="review"]', 'final');
    ok('review tier changes use the agent action',
       (await actions()).some((a) => a.do === 'review' && a.label === 'grok' && a.review === 'final'));
    await page.click('.team .member[data-label="grok"] button[data-do="promote"]');
    ok('master promotion uses the agent action',
       (await actions()).some((a) => a.do === 'promote' && a.label === 'grok'));
    await page.fill('.team .member[data-label="claude"] input.swap', 'replacement');
    await page.click('.team .member[data-label="claude"] button[data-do="swap"]');
    ok('swapping an agent uses the library name',
       (await actions()).some((a) => a.do === 'swap' && a.label === 'claude' && a.into === 'replacement'));

    const beforeRemove = (await actions()).length;
    await page.click('.team .member[data-label="grok"] button[data-do="remove"]');
    ok('removal waits for a second click', (await actions()).length === beforeRemove);
    await page.click('.team .member[data-label="grok"] button[data-do="remove"]');
    ok('confirmed removal uses the agent action',
       (await actions()).some((a) => a.do === 'remove' && a.label === 'grok'));
    await page.evaluate(() => {
      window.fetch = async (url) => url === '/agents'
        ? { ok: false, status: 409, text: async () => '{"why":"agent is already on this roster"}' }
        : { ok: true, status: 204, text: async () => '' };
    });
    await page.fill('#team-add-name', 'codex');
    await page.click('#team-add button');
    ok('a refused action explains itself and preserves the form',
       (await page.textContent('#team-why')).includes('already on this roster')
       && await page.inputValue('#team-add-name') === 'codex');
    ok('team management has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- auto mode shows effective and inherited state for each agent
  {
    const { browser, page, errors } = await open();
    await page.evaluate(() => {
      window.__autoRefused = true;
      window.fetch = async (url, opts) => {
        window.__frames.push([url, opts?.body]);
        if (url === '/agents/candidates') return { ok: true, json: async () => [] };
        if (url === '/agents' && JSON.parse(opts.body).do === 'auto_mode' && window.__autoRefused)
          return { ok: false, status: 409, text: async () => '{"why":"agent config could not be written"}' };
        return { ok: true, status: 204 };
      };
    });
    await page.evaluate((d) => window.__send('status', d), facts([
      { ...agent('codex', 'master'), tool: 'codex', auto: 'on' },
      { ...agent('grok', 'commit'), tool: 'grok', auto: 'on', auto_explicit: 'on' },
      { ...agent('claude', 'final'), tool: 'claude', auto: 'off', auto_explicit: 'off' },
    ]));
    await page.click('#mode-timeline');
    await page.click('#manage-team');
    for (const name of ['codex', 'grok', 'claude'])
      await page.click(`.team .member[data-label="${name}"] > summary`);
    const row = (name) => `.team .member[data-label="${name}"]`;
    ok('inherited on, explicit on, and explicit off are distinct',
       (await page.textContent(`${row('codex')} .auto-state`)).includes('Auto on · inherited')
       && (await page.textContent(`${row('grok')} .auto-state`)).includes('Auto on · set for this agent')
       && (await page.textContent(`${row('claude')} .auto-state`)).includes('Auto off · set for this agent'));
    await page.click(`${row('codex')} button[data-do="auto_mode"]`);
    await page.waitForFunction(() => document.querySelector('#team-why').textContent.includes('could not be written'));
    ok('a refused write explains itself without changing the displayed mode',
       (await page.textContent(`${row('codex')} .auto-state`)).includes('Auto on · inherited')
       && !(await page.isDisabled(`${row('codex')} button[data-do="auto_mode"]`)));
    await page.evaluate(() => { window.__autoRefused = false; });
    await page.click(`${row('codex')} button[data-do="auto_mode"]`);
    await page.waitForFunction(() => document.querySelector('.member[data-label="codex"] .auto-state').textContent.includes('Auto off'));
    const asks = await page.evaluate(() => window.__frames.filter((f) => f[0] === '/agents')
      .map((f) => JSON.parse(f[1])).filter((a) => a.do === 'auto_mode'));
    ok('a successful toggle writes the requested mode and updates immediately',
       asks.length === 2 && asks.every((a) => a.label === 'codex' && a.mode === 'off')
       && (await page.textContent(`${row('codex')} .auto-state`)).includes('Auto off · set for this agent'));
    await page.click(`${row('claude')} button[data-do="auto_mode"]`);
    await page.waitForFunction(() => document.querySelector('.member[data-label="claude"] .auto-state').textContent.includes('Auto on'));
    ok('off can be turned on', (await page.textContent(`${row('claude')} .auto-state`)).includes('Auto on · set for this agent'));
    ok('auto mode controls have no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- session clearing and pane recovery use separate agent actions
  {
    const { browser, page, errors } = await open();
    await page.evaluate(() => {
      window.__refuseReopen = true;
      window.fetch = async (url, opts) => {
        window.__frames.push([url, opts?.body]);
        if (url === '/agents/candidates') return { ok: true, json: async () => [] };
        if (url === '/agents/reopen') return window.__refuseReopen
          ? { ok: false, status: 409, json: async () => ({ why: 'zellij refused to create a pane' }) }
          : { ok: true, status: 200, json: async () => ({ message: 'pane reopened' }) };
        return { ok: true, status: 204 };
      };
    });
    const team = (session) => facts([
      { ...agent('codex', 'master'), tool: 'codex', session },
      { ...agent('grok', 'commit'), tool: 'grok', session: null },
    ]);
    await page.evaluate((d) => window.__send('status', d), team('session-one'));
    await page.evaluate((d) => window.__send('panes', d), panes(['codex']));
    await page.click('#mode-timeline');
    await page.click('#manage-team');
    await page.click('.team .member[data-label="codex"] > summary');
    await page.click('.team .member[data-label="grok"] > summary');
    const codex = '.team .member[data-label="codex"]';
    const grok = '.team .member[data-label="grok"]';
    const clears = () => page.evaluate(() => window.__frames.filter((f) => f[0] === '/agents'
      && JSON.parse(f[1]).do === 'clear_session').map((f) => JSON.parse(f[1])));
    ok('a live pane offers session clearing but no reopen',
       await page.locator(`${codex} button[data-do="clear_session"]`).count() === 1
       && await page.locator(`${codex} button[data-do="reopen"]`).count() === 0);
    ok('an unbound missing pane offers reopen but no clearing',
       await page.locator(`${grok} button[data-do="clear_session"]`).count() === 0
       && await page.locator(`${grok} button[data-do="reopen"]`).count() === 1);
    await page.click(`${codex} button[data-do="clear_session"]`);
    ok('clearing waits for confirmation', (await clears()).length === 0);
    await page.evaluate((d) => window.__send('status', d), team('session-two'));
    ok('a changed binding cancels the old confirmation',
       await page.textContent(`${codex} button[data-do="clear_session"]`) === 'Clear session');
    await page.click(`${codex} button[data-do="clear_session"]`);
    ok('the new binding also needs confirmation', (await clears()).length === 0);
    await page.click(`${codex} button[data-do="clear_session"]`);
    ok('clear names the confirmed binding', (await clears()).length === 1
       && (await clears())[0].session === 'session-two');
    await page.click(`${grok} button[data-do="reopen"]`);
    await page.waitForFunction(() => document.querySelector('#team-why').textContent.includes('zellij refused'));
    ok('reopen refusal is shown beside the action', (await page.textContent('#team-why')).includes('zellij refused'));
    await page.evaluate(() => { window.__refuseReopen = false; });
    await page.click(`${grok} button[data-do="reopen"]`);
    await page.waitForFunction(() => document.querySelector('#team-why').textContent.includes('pane reopened'));
    await page.evaluate((d) => window.__send('panes', d), panes(['codex', 'grok']));
    ok('the live pane update removes reopen', await page.locator(`${grok} button[data-do="reopen"]`).count() === 0);
    ok('session and pane controls have no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

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

  // ---- submitted chat messages wait for transcript proof
  {
    const { browser, page, errors } = await open();
    const grok = (session) => facts([{ ...agent('grok', 'master', null, null, true), session, tool: 'grok' }]);
    await page.evaluate((d) => window.__send('status', d), grok('s1'));
    await page.evaluate((d) => window.__send('panes', d), panes(['grok']));
    await page.evaluate((d) => window.__send('turns', d), turns('grok', [
      text('old', 'person', 'hello', 'hello'),
    ]));
    await page.evaluate(() => {
      window.__sayReply = null;
      window.fetch = (url) => url === '/say'
        ? new Promise((resolve) => { window.__sayReply = resolve; })
        : Promise.resolve({ ok: true, status: 204, text: async () => '' });
    });
    await page.fill('#text', 'hello');
    await page.click('#send');
    ok('submitted text appears while /say is in flight',
       await page.locator('.turn.outgoing').count() === 1
       && (await page.textContent('.turn.outgoing')).includes('Sending'));
    await page.evaluate(() => window.__sayReply({ ok: true, status: 204, text: async () => '' }));
    await page.waitForTimeout(40);
    ok('a send response still waits for transcript',
       (await page.textContent('.turn.outgoing')).includes('Waiting for Chat history'));
    await page.evaluate((d) => window.__send('status', d), facts([
      { ...agent('grok', 'master', null, null, true), session: 's1', tool: 'grok' },
      agent('codex', 'commit', { verb: 'reviewing', object: 'work', since: Math.floor(Date.now()/1000) - 10 }),
    ]));
    ok('pending submission keeps its recipient on screen', await page.textContent('#whoname') === 'grok');
    await page.click('#who');
    await page.click('.picker .opt[data-key="pin:grok"]');
    await page.evaluate((d) => window.__send('turns', d), turns('grok', [
      text('old', 'person', 'hello', 'hello'),
    ]));
    ok('an old identical turn cannot settle the new send', await page.locator('.turn.outgoing').count() === 1);
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('new', 'person', 'hello', 'hello'),
    });
    ok('the recorded message replaces the pending copy',
       await page.locator('.turn.outgoing').count() === 0
       && await page.locator('.turn.person').count() === 2);

    await page.evaluate(() => {
      window.fetch = async () => ({ ok: true, status: 204, text: async () => '' });
    });
    for (let i = 0; i < 2; i++) {
      await page.fill('#text', 'same words');
      await page.click('#send');
      await page.waitForTimeout(30);
    }
    ok('identical submissions have separate pending rows', await page.locator('.turn.outgoing').count() === 2);
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('same-1', 'person', 'same words', 'same words'),
    });
    ok('one recorded turn settles only one submission', await page.locator('.turn.outgoing').count() === 1);
    await page.evaluate((d) => window.__send('turns', d), turns('grok', [
      text('old', 'person', 'hello', 'hello'),
      text('new', 'person', 'hello', 'hello'),
      text('same-1', 'person', 'same words', 'same words'),
    ]));
    ok('a replay cannot settle the other identical submission', await page.locator('.turn.outgoing').count() === 1);
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('same-2', 'person', 'same words', 'same words'),
    });
    ok('both settled copies appear once', await page.locator('.turn.outgoing').count() === 0
       && await page.locator('.turn.person').count() === 4);

    await page.evaluate(() => {
      window.fetch = async () => ({ ok: false, status: 502, text: async () => 'pane unavailable' });
    });
    await page.fill('#text', 'retry me');
    await page.click('#send');
    await page.waitForTimeout(40);
    ok('failure appears beside the message', (await page.textContent('.turn.outgoing.failed')).includes('pane unavailable'));
    ok('the failed message remains editable for retry', await page.inputValue('#text') === 'retry me');
    await page.fill('#text', '');
    await page.click('.turn.outgoing.failed .retry');
    ok('the failed row can restore its text for retry', await page.inputValue('#text') === 'retry me');
    await page.evaluate(() => {
      window.fetch = async () => ({ ok: true, status: 204, text: async () => '' });
    });
    await page.click('#send');
    await page.waitForTimeout(40);
    ok('retry reuses the failed row', await page.locator('.turn.outgoing').count() === 1
       && (await page.textContent('.turn.outgoing')).includes('Waiting for Chat history'));
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('retried', 'person', 'retry me', 'retry me'),
    });
    ok('the retry settles without a duplicate', await page.locator('.turn.outgoing').count() === 0);

    await page.evaluate(() => {
      window.fetch = async () => ({ ok: false, status: 502, text: async () => 'transport lost' });
    });
    await page.fill('#text', 'late proof');
    await page.click('#send');
    await page.waitForTimeout(40);
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('late', 'person', 'late proof', 'late proof'),
    });
    ok('transcript proof clears a late send failure', await page.locator('.turn.outgoing').count() === 0
       && await page.inputValue('#text') === ''
       && (await page.textContent('#sent')).includes('Recorded in Chat history'));
    await page.evaluate(() => {
      window.fetch = async () => ({ ok: true, status: 204, text: async () => '' });
    });
    await page.fill('#text', 'survive refresh');
    await page.click('#send');
    await page.waitForTimeout(40);
    await page.evaluate(() => localStorage.removeItem('clank.selection'));
    await page.reload();
    await page.waitForFunction(() => !!window.__sock);
    const competing = (session) => facts([
      { ...agent('grok', 'master', null, null, true), session, tool: 'grok' },
      agent('codex', 'commit', { verb: 'reviewing', object: 'work', since: Math.floor(Date.now()/1000) - 10 }),
    ]);
    await page.evaluate((d) => window.__send('status', d), competing('s1'));
    await page.evaluate((d) => window.__send('panes', d), panes(['grok']));
    await page.evaluate((d) => window.__send('turns', d), turns('grok', []));
    ok('refresh retains the pending message for the same session',
       await page.textContent('#whoname') === 'grok'
       && (await page.textContent('.turn.outgoing')).includes('survive refresh'));
    await page.evaluate((d) => window.__send('status', d), competing('s2'));
    await page.evaluate((d) => window.__send('turns', d), { agent: 'grok', session: 's2', generation: 1, turns: [] });
    ok('a new session cannot inherit the pending message', await page.locator('.turn.outgoing').count() === 0
       && await page.textContent('#whoname') === 'codex');
    await page.click('#who');
    await page.click('.picker .opt[data-key="pin:grok"]');
    await page.fill('#text', 'dismiss me');
    await page.click('#send');
    await page.waitForTimeout(40);
    ok('an unconfirmed send can be dismissed', await page.locator('.turn.outgoing').count() === 1);
    await page.click('.turn.outgoing .dismiss');
    ok('dismiss removes the pending row', await page.locator('.turn.outgoing').count() === 0);
    ok('pending chat has no script errors', errors.length === 0, errors.join('; '));
    await browser.close();
  }

  // ---- blocked storage cannot prevent a chat submission
  {
    const { browser, page, errors } = await open(true);
    await page.evaluate((d) => window.__send('status', d), facts([
      { ...agent('grok', 'master', null, null, true), session: 's1', tool: 'grok' },
    ]));
    await page.evaluate((d) => window.__send('panes', d), panes(['grok']));
    await page.evaluate((d) => window.__send('turns', d), turns('grok', []));
    await page.fill('#text', 'still send');
    await page.click('#send');
    await page.waitForTimeout(40);
    ok('storage refusal still sends chat', await page.evaluate(() => window.__frames.some((f) => f[0] === '/say')));
    ok('storage refusal still shows pending', await page.locator('.turn.outgoing').count() === 1);
    ok('storage refusal has no script errors', errors.length === 0, errors.join('; '));
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

  // ---- Grok uses the transcript, sends through the same composer,
  // and keeps its terminal view reachable.
  {
    const { browser, page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([
      { ...agent('grok', 'master', null, null, true), tool: 'grok' },
    ]));
    await page.evaluate((d) => window.__send('panes', d), panes(['grok']));
    await page.evaluate((d) => window.__send('turns', d), turns('grok', [
      text('grok:0:user', 'person', 'earlier question', 'earlier question'),
      text('grok:61:assistant', 'agent', 'earlier answer', 'earlier answer'),
    ]));
    ok('Grok opens in chat mode', await page.isVisible('.view.transcript.on'));
    ok('Grok history is readable', (await page.textContent('.view.transcript.on')).includes('earlier answer'));
    await page.fill('#text', 'new question');
    await page.click('#send');
    await page.waitForTimeout(40);
    const sent = await page.evaluate(() => window.__frames.find((f) => f[0] === '/say'));
    ok('Grok receives the submitted message', sent && JSON.parse(sent[1]).pane === 'pane-grok' && JSON.parse(sent[1]).text === 'new question');
    await page.evaluate((d) => window.__send('turn', d), {
      agent: 'grok', session: 's1', generation: 1,
      turn: text('grok:200:assistant', 'agent', 'reply from Grok', 'reply from Grok'),
    });
    ok('the resulting Grok reply appears', (await page.textContent('.view.transcript.on')).includes('reply from Grok'));
    await page.click('#mode-terminal');
    ok('the Grok terminal stays available', await page.isVisible('.view.terminal.on'));
    await page.click('#mode-transcript');
    await page.evaluate(() => {
      window.fetch = async () => ({ ok: false, status: 502, text: async () => 'Grok pane refused the message' });
    });
    await page.fill('#text', 'fail');
    await page.click('#send');
    await page.waitForTimeout(40);
    ok('a Grok send failure names its reason', (await page.textContent('#sent')).includes('Grok pane refused the message'));
    await page.evaluate(() => {
      window.fetch = async () => ({ ok: false, status: 502, text: async () => 'Grok interrupt could not be delivered' });
    });
    await page.click('#stop');
    await page.waitForTimeout(40);
    ok('a Grok interrupt failure names its reason', (await page.textContent('#sent')).includes('Grok interrupt could not be delivered'));
    ok('a failed Grok Stop can be retried', await page.isVisible('#stop'));
    await page.evaluate(() => {
      window.fetch = async () => ({ ok: true, status: 204, text: async () => '' });
    });
    await page.click('#stop');
    ok('a delivered Grok Stop settles briefly', await page.isHidden('#stop'));
    ok('Grok Stop explains when to retry', (await page.textContent('#sent')).includes('if it cleared a draft'));
    await page.waitForTimeout(2100);
    ok('Grok Stop can be tried again if a draft absorbed Ctrl+C', await page.isVisible('#stop'));
    ok('Grok chat has no script errors', errors.length === 0, errors.join('; '));
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
    // 80f01612 removed workingOf(shown) from Stop visibility, leaving
    // the old stamp/tie assertions stale. Stop now follows the live
    // pane while Chat's stamp and turns arrive on separate cadences.
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
    ok('an open pane offers Stop even before Chat activity arrives', await page.isVisible('#stop'));

    // The case that made this evidence rather than a verdict: a turn
    // arrives with NO new status frame behind it.
    await spoke(now + 5, 't-live');
    ok('a turn alone brings Stop back', await page.isVisible('#stop'),
       'no status frame followed it');
    ok('and Send is untouched', !(await page.isDisabled('#send')));

    await say(true, now + 5);
    ok('a Chat timestamp tie does not hide the pane Stop control', await page.isVisible('#stop'));
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
    ok('and names the refusal', (await page.textContent('#sent')).includes('Not stopped: boom'));
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
    ok('and names the failure', (await page.textContent('#sent')).includes('Not sent: no'));
    ok('no errors around the two races', errors.length === 0, errors.join('; '));
  }

  // ---- a pasted image is an attachment
  {
    const { page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master')]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await page.evaluate(() => {
      window.__n = 0; window.__ups = []; window.__uploaded = [];
      window.fetch = (url, opts) => {
        if (url === '/upload') {
          const n = ++window.__n;
          window.__uploaded.push(opts.headers['content-type']);
          return new Promise((res) => { window.__ups.push(() => res({ ok: true, status: 200, json: async () => ({ path: `/a/${n}.png` }) })); });
        }
        return Promise.resolve({ ok: true, status: 204, text: async () => '' });
      };
    });
    // A real paste event, carrying a real file.
    const paste = (kinds) => page.evaluate((list) => {
      const dt = new DataTransfer();
      for (const k of list) {
        if (k === 'text') dt.setData('text/plain', 'just words');
        else dt.items.add(new File([new Uint8Array([1, 2, 3])], `${k}.png`, { type: 'image/png' }));
      }
      document.querySelector('#text').dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
    }, kinds);

    await paste(['shot']);
    await page.waitForFunction(() => window.__ups.length === 1);
    ok('a pasted image is uploaded', await page.evaluate(() => window.__uploaded[0]) === 'image/png');
    ok('and holds the draft while it goes', await page.isDisabled('#send'));
    await page.evaluate(() => window.__ups.shift()());
    await page.waitForTimeout(60);
    ok('its path lands in the box', (await page.inputValue('#text')) === '/a/1.png', await page.inputValue('#text'));

    // Two at once, named in the order they were pasted.
    await page.fill('#text', '');
    const base = await page.evaluate(() => window.__n);
    await paste(['one', 'two']);
    await page.waitForFunction(() => window.__ups.length === 1);
    ok('the second waits for the first', await page.evaluate(() => window.__n) === base + 1,
       'or two uploads race and the paths land out of order');
    await page.evaluate(() => window.__ups.shift()());
    await page.waitForFunction(() => window.__ups.length === 1);
    await page.evaluate(() => window.__ups.shift()());
    await page.waitForTimeout(60);
    ok('two pasted images name two paths, in order',
       (await page.inputValue('#text')) === '/a/2.png /a/3.png', await page.inputValue('#text'));

    // Text is the browser's business.
    await page.fill('#text', '');
    const before = await page.evaluate(() => window.__n);
    await paste(['text']);
    await page.waitForTimeout(60);
    ok('a text paste attaches nothing', await page.evaluate(() => window.__n) === before);
    ok('and is not swallowed', await page.evaluate(() => {
      const dt = new DataTransfer();
      dt.setData('text/plain', 'words');
      const ev = new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true });
      document.querySelector('#text').dispatchEvent(ev);
      return !ev.defaultPrevented;
    }));

    // A refused paste behaves like a refused `+`.
    await page.evaluate(() => { window.fetch = async () => ({ ok: false, status: 413, text: async () => 'too big', json: async () => ({ path: '/a/nope.png' }) }); });
    await paste(['huge']);
    await page.waitForTimeout(80);
    ok('a refused paste leaves the box alone', (await page.inputValue('#text')) === '');
    ok('and says so', (await page.textContent('#sent')).includes('Not attached'));
    ok('and Send recovers', !(await page.isDisabled('#send')));
    // A browser that fills `items` but not `files` — the fallback the
    // real surface differs on. Chromium fills both, so the branch
    // needs a clipboard of its own to be exercised at all.
    await page.evaluate(() => { window.__n = 0; window.__ups = []; window.fetch = (url, opts) => {
      if (url === '/upload') { const n = ++window.__n; return new Promise((res) => { window.__ups.push(() => res({ ok: true, status: 200, json: async () => ({ path: `/b/${n}.png` }) })); }); }
      return Promise.resolve({ ok: true, status: 204, text: async () => '' }); }; });
    await page.fill('#text', '');
    await page.evaluate(() => {
      const file = new File([new Uint8Array([9])], 'i.png', { type: 'image/png' });
      const ev = new Event('paste', { bubbles: true, cancelable: true });
      Object.defineProperty(ev, 'clipboardData', {
        value: { files: [], items: [{ kind: 'file', getAsFile: () => file }] },
      });
      document.querySelector('#text').dispatchEvent(ev);
    });
    await page.waitForFunction(() => window.__ups.length === 1);
    await page.evaluate(() => window.__ups.shift()());
    await page.waitForTimeout(60);
    ok('a clipboard with only `items` still attaches', (await page.inputValue('#text')) === '/b/1.png',
       await page.inputValue('#text'));
    ok('and exactly once', await page.evaluate(() => window.__n) === 1);

    // And the other way round: a browser that fills `files` and
    // leaves `items` empty. Both surfaces exist in the wild and
    // Chromium fills both, so neither branch is exercised by a real
    // clipboard here.
    await page.fill('#text', '');
    await page.evaluate(() => {
      const file = new File([new Uint8Array([9])], 'f.png', { type: 'image/png' });
      const ev = new Event('paste', { bubbles: true, cancelable: true });
      Object.defineProperty(ev, 'clipboardData', { value: { files: [file], items: [] } });
      document.querySelector('#text').dispatchEvent(ev);
    });
    await page.waitForFunction(() => window.__ups.length === 1);
    await page.evaluate(() => window.__ups.shift()());
    await page.waitForTimeout(60);
    ok('a clipboard with only `files` attaches too', (await page.inputValue('#text')) === '/b/2.png',
       await page.inputValue('#text'));
    ok('and still exactly once', await page.evaluate(() => window.__n) === 2);

    ok('the native file picker stays out of sight', await page.isHidden('#file'));
    ok('while + still reaches it', await page.evaluate(() => {
      let opened = false;
      const f = document.querySelector('#file');
      const was = f.click; f.click = () => { opened = true; };
      document.querySelector('#attach').click();
      f.click = was;
      return opened;
    }));
    ok('no errors around pasting', errors.length === 0, errors.join('; '));
  }

  // ---- a pasted batch is one hold, however its files fare
  {
    const { page, errors } = await open();
    const now = Math.floor(Date.now() / 1000);
    const two = (claudeWorks) => page.evaluate(([n, w]) => window.__send('status', {
      facts: {
        lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [
          { label: 'claude', role: 'master', owes: w ? { verb: 'working', object: 'x', since: n - 9 } : null, last: null, transcript: false, ended: null },
          { label: 'codex', role: 'commit', owes: w ? null : { verb: 'reviewing', object: 'y', since: n - 3 }, last: null, transcript: false, ended: null },
        ],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }), [now, claudeWorks]);
    await two(true);
    await page.evaluate((d) => window.__send('panes', d), panes(['claude', 'codex']));
    ok('auto is on claude', (await page.textContent('#whoname')) === 'claude');

    await page.evaluate(() => {
      window.__n = 0; window.__settle = [];
      window.fetch = (url, opts) => {
        if (url === '/upload') { const n = ++window.__n; return new Promise((ok_, no) => { window.__settle.push({ ok: () => ok_({ ok: true, status: 200, json: async () => ({ path: `/c/${n}.png` }) }), bad: () => ok_({ ok: false, status: 413, text: async () => 'no', json: async () => ({ path: '/c/x.png' }) }) }); }); }
        if (url === '/say') { window.__said = JSON.parse(opts.body); return Promise.resolve({ ok: true, status: 204, text: async () => '' }); }
        return Promise.resolve({ ok: true, status: 204, text: async () => '' });
      };
    });
    await page.evaluate(() => {
      const dt = new DataTransfer();
      for (const n of ['a', 'b']) dt.items.add(new File([new Uint8Array([1])], `${n}.png`, { type: 'image/png' }));
      document.querySelector('#text').dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
    });
    await page.waitForFunction(() => window.__settle.length === 1);

    // The turn moves while the batch is still going up.
    await two(false);
    // The FIRST file fails, which leaves the box empty.
    await page.evaluate(() => window.__settle.shift().bad());
    await page.waitForTimeout(60);
    ok('a failed file does not release the batch', (await page.textContent('#whoname')) === 'claude',
       'or the next image lands in front of another agent');
    await page.waitForFunction(() => window.__settle.length === 1);
    await page.evaluate(() => window.__settle.shift().ok());
    await page.waitForTimeout(60);
    ok('the surviving file lands on the agent it was pasted to', (await page.textContent('#whoname')) === 'claude');
    ok('and its path is in the box', (await page.inputValue('#text')) === '/c/2.png', await page.inputValue('#text'));
    await page.click('#send');
    await page.waitForTimeout(60);
    ok('so the message goes where it was written', await page.evaluate(() => window.__said.pane) === 'pane-claude',
       JSON.stringify(await page.evaluate(() => window.__said)));

    // Every file failing still gives the composer back.
    await page.fill('#text', '');
    await page.evaluate(() => {
      const dt = new DataTransfer();
      dt.items.add(new File([new Uint8Array([1])], 'c.png', { type: 'image/png' }));
      document.querySelector('#text').dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
    });
    await page.waitForFunction(() => window.__settle.length === 1);
    await page.evaluate(() => window.__settle.shift().bad());
    await page.waitForTimeout(60);
    ok('a batch that all failed releases the hold', (await page.textContent('#whoname')) === 'codex');
    ok('and Send is usable again', !(await page.isDisabled('#send')) || (await page.inputValue('#text')) === '');
    ok('no errors around the batch', errors.length === 0, errors.join('; '));
  }

  // ---- the page says which repo it is about
  {
    const { page, errors } = await open();
    ok('before any frame the tab is not a session name', await page.title() === 'clank',
       await page.title());
    ok('and nothing claims to be a project yet', await page.isHidden('#project'));

    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master')]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    // Deliberately not `clank`: that is the page's own fallback, and
    // a check using it cannot tell the project from the default.
    ok('the top bar names the project', (await page.textContent('#project')) === 'the-repo',
       await page.textContent('#project'));
    ok('and so does the tab', await page.title() === 'the-repo', await page.title());

    // The session is a different question, asked in the ledger.
    ok('the ledger still names the session', (await page.textContent('#session')).length > 0,
       await page.textContent('#session'));

    // A name is whatever a directory can be called, so it goes in as
    // text and never as markup.
    await page.evaluate(() => window.__send('status', {
      facts: {
        project: '<img src=x onerror=alert(1)>', lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: null, last: null, transcript: false, ended: null }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }));
    ok('an awkward name is shown, not run', (await page.textContent('#project')) === '<img src=x onerror=alert(1)>');
    ok('and no element came of it', await page.locator('#project img').count() === 0);

    // Long names, narrow screen: the header still fits, and the agent
    // keeps its place while the project gives way.
    await page.evaluate(() => window.__send('status', {
      facts: {
        project: 'a-repository-with-a-really-quite-long-directory-name', lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'a-very-long-agent-label-indeed', role: 'master', owes: null, last: null, transcript: false, ended: null }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
      },
    }));
    for (const w of [320, 390]) {
      await page.setViewportSize({ width: w, height: 800 });
      await page.waitForTimeout(40);
      const m = await page.evaluate(() => ({
        doc: document.documentElement.scrollWidth,
        timeline: Math.round(document.querySelector('#mode-timeline').getBoundingClientRect().right),
        name: Math.round(document.querySelector('#whoname').getBoundingClientRect().width),
      }));
      ok(`long names still fit at ${w}px`, m.doc <= w && m.timeline <= w, JSON.stringify(m));
      ok(`and the agent keeps room at ${w}px`, m.name > 40, JSON.stringify(m));
    }
    ok('no errors around naming', errors.length === 0, errors.join('; '));
  }

  // ---- a plan that is one commit reads as one line
  {
    const { page, errors } = await open();
    const ledger = (rows) => page.evaluate((list) => window.__send('status', {
      facts: {
        project: 'the-repo', lamp: 'l', plan: 'foo', hue: '#5f5fff', correction: null,
        agents: [{ label: 'claude', role: 'master', owes: null, last: null, transcript: false, ended: null }],
        ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: list },
      },
    }), rows);

    await ledger([
      { kind: 'commit', glyph: '⚑', sha: 'abc1230', text: 'solo', stands_for: 'solo', author: null, verdict: null, age: '2m', page: 'commit/abc1230.html' },
      { kind: 'header', glyph: null, sha: null, text: 'duo', author: null, verdict: null, age: null, page: 'plan/duo.html' },
      { kind: 'commit', glyph: '⚑', sha: 'def4560', text: 'wrap it up', author: null, verdict: null, age: '9m', page: 'commit/def4560.html' },
    ]);
    await page.click('#mode-timeline');
    await page.waitForTimeout(40);

    const compact = page.locator('.row.commit').first();
    ok('a collapsed row wears the header\'s emphasis',
       await compact.locator('.plan').count() === 1);
    ok('and the name is the plan\'s', (await compact.locator('.plan').textContent()) === 'solo');
    ok('with its sha still beside it', (await compact.locator('.sha').textContent()) === 'abc1230');
    ok('and its own commit page, not the plan\'s',
       (await compact.locator('a').getAttribute('href')) === 'html/commit/abc1230.html',
       await compact.locator('a').getAttribute('href'));

    const plain = page.locator('.row.commit').nth(1);
    ok('an ordinary commit still shows its subject',
       (await plain.textContent()).includes('wrap it up'));
    ok('and wears no plan emphasis', await plain.locator('.plan').count() === 0);
    ok('the two-commit plan keeps its header', await page.locator('.row.header').count() === 1);
    ok('no errors around the ledger', errors.length === 0, errors.join('; '));
  }

  // ---- the composer is a bubble under the column it belongs to
  {
    const { page, errors } = await open();
    await page.evaluate((d) => window.__send('status', d), facts([agent('claude', 'master', null, null, true)]));
    await page.evaluate((d) => window.__send('panes', d), panes(['claude']));
    await page.evaluate((d) => window.__send('turns', d), turns('claude', []));
    await page.setViewportSize({ width: 1400, height: 900 });
    await page.waitForTimeout(40);
    const m = await page.evaluate(() => {
      const r = (s) => { const b = document.querySelector(s).getBoundingClientRect(); return { l: Math.round(b.left), r: Math.round(b.right), w: Math.round(b.width) }; };
      const input = r('#text'), bubble = r('.bubble');
      return { bubble, turns: r('.turns'), input, send: r('#send'), attach: r('#attach') };
    });
    ok('the composer and the reading column are the same column',
       Math.abs(m.bubble.l - m.turns.l) <= 1 && Math.abs(m.bubble.r - m.turns.r) <= 1, JSON.stringify(m));
    ok('and it is wider than it was', m.turns.w > 700, JSON.stringify(m));
    ok('the writing area has the whole width', m.input.w > m.bubble.w - 40, JSON.stringify(m));
    ok('with the controls on the row beneath it', m.attach.l < m.send.l && m.attach.l < m.input.r, JSON.stringify(m));
    ok('no errors around the bubble', errors.length === 0, errors.join('; '));
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
