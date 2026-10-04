// Real computed-style acceptance: status changes must not recolour prose,
// navigation, controls, questions, focus borders or historical verdicts.
const assert = require('node:assert/strict');
const { open, done, facts, agent, panes, text, tool, turns } = require('./harness');

(async () => {
  try {
    const { page, errors } = await open();
    const roster = [{ ...agent('codex', 'master', {verb:'implementing', object:'the plan'}, null, true),
      tool:'codex', session:'s1', auto:'on' }];
    const ledger = { plan:'A stable interface', gate:'continued', dirty:'+1 −1',
      queue:[{name:'A queued plan',handle:'450-plan.md',priority:450,page:'queue/plan.html'}],
      stash:1, stash_entries:[{stem:'A stashed plan',commits:2,ready:true,page:'stash/plan.html'}],
      blocks:[{agent:'codex',name:'choice',question:'Choose a direction'}],
      rows:['v-continue','v-finished','v-changes','v-unmarked',''].map(verdict_class =>
        ({kind:'review',author:'reviewer',glyph:'✓',text:'Recorded verdict',verdict_class})) };
    const status = (hue, gate='continued') => facts(roster, {hue,ledger:{...ledger,gate}});
    await page.evaluate(d => window.__send('status',d), status('#46be77'));
    await page.evaluate(d => window.__send('panes',d), panes(['codex']));
    const question = tool('q','request_user_input','{}',null);
    question.body.question = {provider:'codex',fields:[{id:'direction',header:'Direction',
      question:'Which direction?',multi_select:false,is_secret:false,is_other:false,
      options:[{label:'North',description:'First direction'}]}]};
    await page.evaluate(d => window.__send('turns',d), turns('codex',[
      text('intro','agent','A link','<h1>Stable heading</h1><p>Ordinary text <a href="https://example.com">a link</a></p>'),
      question,
    ]));
    await page.locator('#mode-transcript').click();
    await page.locator('#text').fill('A draft to preserve');
    await page.locator('#text').focus();

    const styles = () => page.evaluate(() => {
      const targets = ['#project','.bar .who .name','#mode-team','#roster .name',
        '#facts .plan','#facts .gate','#queue .q a','#queue-tools a','#stash-list a',
        '.prose h1','.prose p','.prose a','.question-card h3','.question-terminal',
        '#strip','#strip .block button','.say button.send','.bubble','.row.review .glyph'];
      return targets.flatMap(selector => [...document.querySelectorAll(selector)].map((el,index) => {
        const style = getComputedStyle(el);
        return {selector,index,color:style.color,background:style.backgroundColor,
          border:style.borderColor,outline:style.outlineColor};
      }));
    });
    const indicators = () => page.evaluate(() => ({
      activity:getComputedStyle(document.querySelector('#doing .dot')).backgroundColor,
      gate:getComputedStyle(document.querySelector('#facts .gate'),'::before').backgroundColor,
    }));
    const baseline = await styles();
    for (const selector of ['.prose a','.question-card h3','#queue .q a','#strip .block button',
      '.row.review .glyph']) assert(baseline.some(x => x.selector===selector),`missing fixture ${selector}`);
    const initial = await indicators();
    for(const [hue,gate] of [['#e8c547','reviewing'],['#d05a4a','changes requested'],
      ['#5f5fff','unreviewed'],[null,'continued'],['#46be77','finished']]) {
      await page.evaluate(d => window.__send('status',d),status(hue,gate));
      assert.deepEqual(await styles(),baseline,`unrelated style changed for ${gate}`);
      if(hue && hue!=='#46be77') assert.notDeepEqual(await indicators(),initial,'status indicators should change');
      assert.equal(await page.locator('#facts .gate').textContent(),gate);
      assert.equal(await page.locator('#text').inputValue(),'A draft to preserve');
    }
    // Text/link/button contrast against the actual fixed page palette. Canvas
    // resolves color-mix into RGB, without assuming a browser serialization.
    const contrasts = await page.evaluate(() => {
      const ctx = document.createElement('canvas').getContext('2d');
      const rgb = color => { ctx.clearRect(0,0,1,1); ctx.fillStyle=color; ctx.fillRect(0,0,1,1);
        return [...ctx.getImageData(0,0,1,1).data].slice(0,3); };
      const luminance = color => rgb(color).map(v => {v/=255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4;})
        .reduce((n,v,i) => n+v*[.2126,.7152,.0722][i],0);
      const ratio = (a,b) => {const x=luminance(a),y=luminance(b);return (Math.max(x,y)+.05)/(Math.min(x,y)+.05);};
      const root = getComputedStyle(document.documentElement);
      const base = root.getPropertyValue('--base'), bezel=root.getPropertyValue('--bezel');
      const result = {};
      for(const selector of ['.prose a','#queue .q a','#facts .gate','#stash-list a'])
        result[selector] = ratio(getComputedStyle(document.querySelector(selector)).color,
          selector.startsWith('.prose')?base:bezel);
      const send = getComputedStyle(document.querySelector('.say button.send'));
      result.send = ratio(send.color,send.backgroundColor);
      return result;
    });
    for(const [name,contrast] of Object.entries(contrasts)) assert(contrast>=4.5,`${name} contrast ${contrast}`);
    for(const width of [320,390,768,1280]) {
      await page.setViewportSize({width,height:850});
      const atWidth=await styles();
      await page.evaluate(d => window.__send('status',d),status('#e8c547','reviewing'));
      assert.deepEqual(await styles(),atWidth,`state colour leaked at ${width}px`);
      assert(await page.evaluate(() => document.documentElement.scrollWidth<=innerWidth));
      if(width===320) await page.locator('#mode-timeline').click();
      // Wait for layout/compositor before producing optional review previews.
      if(process.env.CLANK_COLOR_PREVIEW) {
        await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
        await page.screenshot({path:`${process.env.CLANK_COLOR_PREVIEW}-${width}.png`});
      }
      if(width===320) await page.locator('#mode-transcript').click();
      await page.evaluate(d => window.__send('status',d),status('#46be77','finished'));
    }
    assert.deepEqual(errors,[]);
    console.log('PASS: computed colours remain stable across repository states and responsive widths; status dots change, text and button contrast passes, drafts and verdicts stay intact.');
  } finally { await done(); }
})().catch(error => {console.error(error);process.exitCode=1;});
