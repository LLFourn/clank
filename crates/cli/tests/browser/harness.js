// Serves the real page.html over a real origin, with a stub socket and
// a stub Terminal, so the page's own code runs against synthetic frames.
const { chromium } = require('playwright');
const http = require('http');
const fs = require('fs');
const path = require('path');

const PAGE = fs.readFileSync(path.join(__dirname, '../../src/cli/web/page.html'), 'utf8')
  .replace(/<link[^>]*>/g, '')
  .replace(/<script src=[^>]*><\/script>/g, '<script>window.Terminal=function(){return{open(){},write(){},resize(){},scrollToBottom(){}}};</script>');

const STUB = `
  window.__frames = [];
  window.WebSocket = class {
    constructor() { window.__sock = this; setTimeout(() => this.onopen && this.onopen(), 0); }
    close() {}
  };
  window.__send = (event, data) => window.__sock.onmessage({ data: 'event: ' + event + '\\ndata: ' + JSON.stringify(data) });
  window.fetch = async (url, opts) => {
    window.__frames.push([url, opts && opts.body]);
    return { ok: true, status: 200, text: async () => 'ok' };
  };
`;

const facts = (agents, extra = {}) => ({
  facts: {
    project: 'the-repo', lamp: 'lamp', plan: 'foo', hue: '#5f5fff', correction: null,
    agents, ledger: { plan: 'foo', gate: 'unreviewed', dirty: null, queue: 0, stash: 0, blocks: [], rows: [] },
    ...extra,
  },
});
const agent = (label, role, owes = null, last = null, transcript = false, ended = null) => ({ label, role, owes, last, transcript, ended });
const turn = (id, who, body, at) => ({ id, who, at: at || Math.floor(Date.now()/1000), body });
const text = (id, who, md, html) => turn(id, who, { kind: 'text', text: md, html });
const tool = (id, name, input, output) => turn(id, 'agent', { kind: 'tool', name, input, output, images: [] });
const turns = (agent, list) => ({ agent, session: 's1', generation: 1, turns: list });

const panes = (labels) => ({ panes: labels.map((l) => ({ id: 'pane-' + l, label: l, columns: 80, rows: 24, exited: false })), closed: [] });

let server, port, browser;
// `deny`: a browser that refuses site data, where even READING
// localStorage throws — a private window, or storage blocked.
async function open(deny) {
  if (!server) {
    server = http.createServer((req, res) => { res.setHeader('content-type', 'text/html'); res.end(PAGE); });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    port = server.address().port;
    // CLANK_CHROMIUM points at an already-downloaded build; without
    // it playwright uses whatever `npx playwright install` fetched.
    const exe = process.env.CLANK_CHROMIUM;
    browser = await chromium.launch(exe ? { executablePath: exe } : {});
  }
  const ctx = await browser.newContext({ viewport: { width: 390, height: 780 } });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await ctx.addInitScript(STUB);
  if (deny) await ctx.addInitScript(`Object.defineProperty(window, 'localStorage', { get() { throw new Error('Access is denied for this document.'); } });`);
  await page.goto(`http://127.0.0.1:${port}/`);
  // A page that died on load never gets here: the socket is the
  // first thing its script does.
  await page.waitForFunction(() => !!window.__sock, null, { timeout: 5000 });
  return { browser: ctx, page, errors };
}
const done = async () => { await browser.close(); server.close(); };
module.exports = { open, done, facts, agent, panes, turn, text, tool, turns };
