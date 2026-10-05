// Requires Node and Playwright with Chromium. Install Playwright separately, then run
// NODE_PATH=/path/to/node_modules node scripts/check-timeline-ui.cjs [preview-directory]
// This serves a fixture copy of the actual page; it never opens an agent session.
const fs = require('fs'), path = require('path'), http = require('http'), assert = require('assert/strict');
const {chromium} = require('playwright');
const root = path.resolve(__dirname, '..'), output = process.argv[2] || '/tmp/clank-timeline-preview';
fs.mkdirSync(output, {recursive:true});
let pageHtml = fs.readFileSync(path.join(root,'crates/web/assets/page.html'),'utf8')
  .replaceAll('__CLANK_BASE_PATH__','/')
  .replaceAll('__CLANK_NONCE__','fixture')
  .replace('/*__CLANK_COMPOSER__*/', fs.readFileSync(path.join(root,'crates/web/assets/composer.js'),'utf8'))
  .replace('connect();', 'window.timelineFixture = {handlers, show, paintTranscript}; connect();');
const requests = [], pictures = new Map(); let refuseMissing=true, delayKey=null, delayReply=null;
const service=fs.readFileSync(path.join(root,'crates/web/src/service.rs'),'utf8');
const policy=service.match(/"(default-src 'none';[^"\n]+)"/)[1].replace('{nonce}','fixture');
const server=http.createServer(async(req,res)=>{
 const url=new URL(req.url,'http://localhost');
 if(url.pathname==='/') {res.writeHead(200,{'content-type':'text/html','content-security-policy':policy});res.end(pageHtml);return;}
 const assets={'/xterm.js':'vendor/xterm.js','/xterm.css':'vendor/xterm.css','/plate.woff2':'vendor/plate.woff2'};
 if(assets[url.pathname]) {res.end(fs.readFileSync(path.join(root,'crates/web/assets',assets[url.pathname])));return;}
 if(url.pathname==='/transcript/image') {
  let body='';for await(const chunk of req)body+=chunk;
  const ask=JSON.parse(body);requests.push(ask);
  if(ask.key===delayKey)await new Promise(resolve=>delayReply=resolve);
  if(ask.key==='missing' && refuseMissing){res.writeHead(404);res.end('The transcript did not retain this image. Reattach it to view it.');return;}
  const pic=pictures.get(ask.key==='missing'?'portrait':ask.key);
  if(!pic){res.writeHead(409);res.end('This attachment has changed.');return;}
  res.setHeader('content-type','application/json');res.end(JSON.stringify(pic));return;
 }
 res.writeHead(204);res.end();
});
(async()=>{
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 const browser=await chromium.launch({headless:true});
 try {
  const page=await browser.newPage({viewport:{width:1280,height:900}}), errors=[];
  page.on('pageerror',e=>errors.push(e.message));
  page.on('console',m=>{if(m.type()==='error' && m.text()!=='Failed to load resource: the server responded with a status of 404 (Not Found)')errors.push(m.text());});
  await page.addInitScript(()=>{
   window.CLANK_WEB_CONFIG={base:'/',hosted:true,login:'/login',signout:null};
   window.WebSocket=class{constructor(){setTimeout(()=>this.onopen?.(),0)}send(){}close(){}};
  });
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(()=>!!window.timelineFixture);
  for(const [key,width,height] of [['portrait',480,1600],['landscape',1600,500]]) {
   pictures.set(key,await page.evaluate(({key,width,height})=>{
    const canvas=document.createElement('canvas');canvas.width=width;canvas.height=height;
    const ctx=canvas.getContext('2d');ctx.fillStyle=key==='portrait'?'#e5ede8':'#e4eaf1';ctx.fillRect(0,0,width,height);
    ctx.fillStyle='#334c41';ctx.font='bold 38px sans-serif';ctx.fillText('Clank attachment',32,70);
    ctx.font='26px sans-serif';ctx.fillText(key+' original',32,116);
    for(let i=0;i<8;i++){ctx.fillStyle=i%2?'#9cb5a7':'#bed0c6';ctx.fillRect(32,170+i*150,width-64,90)}
    return {media_type:'image/png',data:canvas.toDataURL().split(',')[1],width,height};
   },{key,width,height}));
  }
  const text=(id,text)=>({id,who:'person',at:null,body:{kind:'text',text,html:''}});
  const image=(id,key,label)=>({id,who:'person',at:null,body:{kind:'image',media_type:'image/png',bytes:100,key,label}});
  const tools=[
   {id:'grok-tool:1',who:'agent',body:{kind:'tool',name:'run_terminal_command',input:'{"command":"clank wait","block_until_ms":0}',title:'clank wait',detail:'clank wait',output:'<task-id>fixture-task</task-id>\n<task-type>bash</task-type>\n<output-file>/tmp/fixture.log</output-file>\n<status>running</status>\n<summary>Background task started</summary>',tasks:[{id:'fixture-task',status:'running'}]}},
   {id:'grok-tool:2',who:'agent',body:{kind:'tool',name:'run_terminal_command',input:'{"command":"cargo test"}',title:'cargo test',detail:'cargo test',output:'3 tests passed'}},
   {id:'grok-tool:3',who:'agent',body:{kind:'tool',name:'run_terminal_command',input:'{"command":"echo fallback"}',title:'echo fallback',detail:'echo fallback',output:'Unknown provider payload: retained as readable output'}}
  ];
  const turns=[text('before','Please review these screenshots.'),image('portrait-turn','portrait','Image #1'),text('between','The landscape screenshot is next.'),image('landscape-turn','landscape','Image #2'),image('missing-turn','missing','Unavailable attachment'),text('literal','Literal markup: <image name=example> is preserved.'),...tools];
  const frame={agent:'grok',session:'fixture-session',generation:1,turns};
  await page.evaluate(frame=>{
   const f=window.timelineFixture;
   f.handlers.status({facts:{agents:[{label:'grok',tool:'grok',role:'master',session:'fixture-session',last:'idle',auto_mode:true,transcript:true}],ledger:{queue:[],stash_entries:[],rows:[],blocks:[],stash:0},repo:'clank',hue:'#5f5fff'}});
   f.handlers.turns(frame); f.handlers.connection({connected:true}); f.show('grok');
  },frame);
  await page.locator('[data-id="portrait-turn"] .shot-button').scrollIntoViewIfNeeded();
  await page.waitForSelector('[data-id="portrait-turn"] img');
  assert.equal(await page.locator('.attachment').count(),3);
  assert.equal(await page.locator('.did > summary').innerText(),'Running 3 commands…');
  assert.equal(requests.filter(r=>!r.preview).length,0,'originals are not loaded eagerly');
  const thumb=await page.locator('[data-id="portrait-turn"] img').boundingBox();
  assert.ok(thumb.height<=170 && Math.abs(thumb.width/thumb.height - 480/1600)<0.01,JSON.stringify(thumb));
  assert.equal(await page.locator('details.did').getAttribute('open'),null,'command groups start collapsed');
  assert.ok((await page.locator('.turns').innerText()).includes('Literal markup:'));
  await page.screenshot({path:output+'/desktop-timeline.png'});
  await page.locator('.did > summary').click();
  await page.locator('[data-id="grok-tool:1"] .tool > summary').click();
  assert.ok((await page.locator('[data-id="grok-tool:1"] .tool > summary').innerText()).includes('clank wait · running'));
  assert.equal(await page.locator('[data-id="grok-tool:1"] pre.out').isVisible(),false,'registration is not shell output');
  assert.ok(!(await page.locator('[data-id="grok-tool:1"] .tool > summary').innerText()).includes('run_terminal_command'));
  await page.locator('[data-id="grok-tool:3"] .tool > summary').click();
  assert.ok((await page.locator('[data-id="grok-tool:3"] pre.out').innerText()).includes('Unknown provider payload'));
  await page.screenshot({path:output+'/desktop-commands.png'});
  const first=page.locator('[data-id="portrait-turn"] .shot-button');
  await first.scrollIntoViewIfNeeded();await first.focus();await page.keyboard.press('Enter');
  await page.waitForSelector('#viewer img');
  assert.equal(requests.filter(r=>!r.preview).length,1);
  const fit=await page.locator('#viewer img').boundingBox();
  assert.ok(fit.height<=850 && fit.width<=1248,JSON.stringify(fit));
  await page.screenshot({path:output+'/desktop-expanded.png'});
  await page.locator('.image-zoom').click();assert.ok((await page.locator('#viewer').getAttribute('class')).includes('zoomed'));
  await page.keyboard.press('Escape');assert.equal(await page.locator('#viewer').isVisible(),false);
  assert.ok(await first.evaluate(e=>document.activeElement===e),'Escape restores focus');
  const previous=requests.length;
  await page.evaluate(frame=>window.timelineFixture.handlers.turns(frame),frame);
  await page.waitForTimeout(80);assert.equal(requests.length,previous,'replay keeps existing thumbnail nodes');
  const recovered=structuredClone(tools[0]);recovered.body.tasks=[{id:'fixture-task',status:'completed',output:'actual command output'}];
  await page.evaluate(turn=>window.timelineFixture.handlers.turn({agent:'grok',session:'fixture-session',generation:1,turn}),recovered);
  assert.equal(await page.locator('.did > summary').innerText(),'Ran 3 commands');
  assert.equal(await page.locator('[data-id="grok-tool:1"] pre.out').innerText(),'actual command output');
  await page.setViewportSize({width:390,height:844});
  await page.evaluate(()=>window.timelineFixture.show('grok'));
  await first.scrollIntoViewIfNeeded();await page.screenshot({path:output+'/mobile-timeline.png'});
  for(const btn of await page.locator('.shot-button').all()){
   const size=await btn.boundingBox();assert.ok(size.width<=282 && size.height<=182,JSON.stringify(size));
  }
  await first.click();await page.waitForSelector('#viewer img');
  const mobile=await page.locator('#viewer img').boundingBox();
  assert.ok(mobile.width<=358 && mobile.height<=770,JSON.stringify(mobile));
  await page.screenshot({path:output+'/mobile-expanded.png'});
  await page.locator('.image-close').click();
  await page.locator('[data-id="missing-turn"] .shot-button').scrollIntoViewIfNeeded();
  await page.waitForSelector('[data-id="missing-turn"] .image-retry:not([hidden])');
  refuseMissing=false;await page.locator('[data-id="missing-turn"] .image-retry').click();
  await page.waitForSelector('[data-id="missing-turn"] img');
  // An image replaced while the original is loading closes and ignores its late response.
  delayKey='portrait';await first.click();
  await page.waitForFunction(()=>document.querySelector('#viewer .image-stage').textContent==='Loading original…');
  await page.evaluate(()=>window.timelineFixture.handlers.turns({agent:'grok',session:'new-session',generation:2,turns:[]}));
  assert.equal(await page.locator('#viewer').isVisible(),false);
  delayReply?.();await page.waitForTimeout(100);assert.equal(await page.locator('#viewer img').count(),0);
  assert.deepEqual(errors,[]);
  console.log('Timeline browser checks passed; desktop/mobile previews:',output);
 } finally {await browser.close();await new Promise(resolve=>server.close(resolve));}
})().catch(error=>{console.error(error);server.close();process.exitCode=1});
