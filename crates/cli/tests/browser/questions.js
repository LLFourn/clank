// Native question UI against authoritative synthetic owner frames. The native
// transport/serialization is exercised separately in Rust's socket fixtures.
const assert=require('node:assert/strict');
const {open,done,facts,agent,turns,tool,panes}=require('./harness');
const fields=[
 {id:'direction',header:'Direction',question:'Choose a direction',multi_select:false,is_other:true,is_secret:false,
  options:[{label:'North',description:'First'},{label:'South',description:'Second'}]},
 {id:'extras',header:'Extras',question:'Choose extras',multi_select:true,is_other:false,is_secret:false,
  options:[{label:'One',description:'First extra'},{label:'<img src=x onerror="window.injected=true">',description:'Inert words'}]},
 {id:'details',header:'Details',question:'Describe the choice',multi_select:false,is_other:false,is_secret:false,options:[]},
 {id:'private',header:'Private',question:'Private answer',multi_select:false,is_other:false,is_secret:true,options:[]},
];
const request={id:'9223372036854775807',turn:'turn-one',item:'native-call',blocking:false,delivery:'pending',question:{provider:'codex',fields}};
const window={session:'s1',owner:'owner:1',available:true,reason:null,pending:[request],ended:null};
const status=facts([{...agent('codex','master',null,null,false),tool:'codex',session:'s1'},
 {...agent('claude','commit',null,null,true),tool:'claude',session:'claude-session'}]);
(async()=>{try {
 const {page,errors}=await open();await page.setViewportSize({width:1280,height:850});
 await page.evaluate(d=>window.__send('status',d),status);
 await page.evaluate(d=>window.__send('panes',d),panes(['codex','claude']));
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),window);
 await page.locator('#mode-transcript').click();
 const card=page.locator('.question-card'), groups=card.locator('fieldset'), submit=card.locator('.question-submit');
 assert.equal(await card.count(),1,'native question works before transcript history exists');
 assert((await card.textContent()).includes('background question'));
 await groups.nth(0).locator('input[type=radio]').first().check();
 await groups.nth(1).locator('input[type=checkbox]').nth(0).check();
 await groups.nth(1).locator('input[type=checkbox]').nth(1).check();
 await groups.nth(2).locator('textarea').fill('A native draft');
 await groups.nth(3).locator('input[type=password]').fill('private-fixture-value');
 assert.equal(await card.locator('img').count(),0);assert.equal(await page.evaluate(()=>window.injected),undefined);
 if(process.env.CLANK_QUESTION_PREVIEW) {
  for(const width of [1280,320]) {
   await page.setViewportSize({width,height:1100});
   await page.evaluate(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r))));
   await page.screenshot({path:`${process.env.CLANK_QUESTION_PREVIEW}-${width}.png`});
  }
  await page.setViewportSize({width:1280,height:850});
 }
 await groups.nth(2).locator('textarea').focus();
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),window);
 assert.equal(await groups.nth(2).locator('textarea').inputValue(),'A native draft');
 assert(await groups.nth(2).locator('textarea').evaluate(el=>el===document.activeElement),'replay preserves focus');
 // A transcript catch-up uses the stable native card key and keeps controls.
 const recorded=tool('native-call','request_user_input','{}',null);recorded.body.question={provider:'codex',fields};
 await page.evaluate(d=>window.__send('turns',d),turns('codex',[recorded]));
 assert.equal(await card.count(),1,'no duplicate recorded/native card');
 assert.equal(await groups.nth(3).locator('input[type=password]').inputValue(),'private-fixture-value');
 await page.evaluate(()=>{window.__answers=[];window.__answerMode='invalid';window.fetch=async(url,opts)=>{
   if(String(url).endsWith('/questions/answer')) {
    window.__answers.push(JSON.parse(opts.body));
    if(window.__answerMode==='hold')return new Promise(resolve=>window.__releaseAnswer=()=>resolve({ok:true,status:202,json:async()=>({state:'submitted',message:'Answer sent'})}));
    if(window.__answerMode==='unknown')throw new Error('transport lost');
    return {ok:false,status:400,json:async()=>({state:'invalid',message:'The answer was refused without sending.'})};
   } return {ok:true,status:200,json:async()=>({})};
 };});
 await page.evaluate(()=>Object.defineProperty(crypto,'randomUUID',{value:undefined}));
 await submit.click();await page.waitForFunction(()=>document.querySelector('.question-notice').textContent.includes('refused'));
 assert(await submit.isEnabled());assert.equal(await groups.nth(2).locator('textarea').inputValue(),'A native draft');
 const first=await page.evaluate(()=>window.__answers[0]);
 assert.equal(first.id,request.id);assert.equal(first.owner,window.owner);assert.equal(first.session,'s1');
 assert.equal(first.turn,request.turn);assert.equal(first.item,request.item);assert(first.attempt);
 assert.deepEqual(first.answers,{direction:['North'],extras:['One','<img src=x onerror="window.injected=true">'],details:['A native draft'],private:['private-fixture-value']});
 await page.evaluate(()=>window.__answerMode='hold');await submit.click();
 await card.locator('form').evaluate(form=>form.requestSubmit());
 assert.equal(await page.evaluate(()=>window.__answers.length),2,'repeated submit cannot duplicate a sending request');
 assert(await submit.isDisabled());
 await page.locator('#roster [data-label=claude]').click();
 await page.evaluate(d=>window.__send('questions',{agents:{codex:{...d,pending:[],ended:{turn:'turn-one',status:'interrupted'}}}}),window);
 await page.evaluate(()=>window.__releaseAnswer());
 await page.locator('#roster [data-label=codex]').click();await page.locator('#mode-transcript').click();
 await page.waitForFunction(()=>document.querySelector('.question-note').textContent.includes('cancelled'));
 assert(await submit.isDisabled());assert.equal(await groups.nth(2).locator('textarea').inputValue(),'A native draft');
 assert(!(await page.locator('body').textContent()).includes('private-fixture-value'));
 const stored=await page.evaluate(()=>[...Object.values(localStorage),...Object.values(sessionStorage)].join(' '));
 assert(!stored.includes('private-fixture-value'));assert(!stored.includes('A native draft'));
 await card.locator('.question-discard').click();
 assert.equal(await card.locator('input, textarea, form').count(),0,'discard closes the local draft and leaves recorded content');
 // Fresh request: transport failure preserves inputs and forbids blind retry.
 const second={...request,id:'"second-native-id"',turn:'turn-two',item:'second-call',question:{provider:'codex',fields:[fields[2]]}};
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[second]});
 const last=page.locator('.question-card').last();await last.locator('textarea').fill('Keep on failure');
 await page.evaluate(()=>window.__answerMode='unknown');await last.locator('.question-submit').click();
 await page.waitForFunction(()=>[...document.querySelectorAll('.question-note')].some(el=>el.textContent.includes('Delivery is unconfirmed')));
 assert.equal(await last.locator('textarea').inputValue(),'Keep on failure');assert(await last.locator('.question-submit').isDisabled());
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[second]});
 assert(await last.locator('.question-submit').isDisabled(),'reconnect replay does not retry an uncertain answer');
 for(const width of [320,390,768,1280]) {await page.setViewportSize({width,height:850});assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));}
 const third={...second,id:'"third-native-id"',turn:'turn-three',item:'third-call'};
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[third]});
 const thirdCard=page.locator('.question-card').last();await thirdCard.locator('textarea').fill('Keep after native loss');
 await page.evaluate(()=>window.__answerMode='hold');await thirdCard.locator('.question-submit').click();
 const unavailable={...window,available:false,pending:[],reason:'native socket closed'};
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),unavailable);
 await page.evaluate(()=>window.__releaseAnswer());
 await page.waitForFunction(()=>[...document.querySelectorAll('.question-note')].at(-1).textContent.includes('Delivery is unconfirmed'));
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),unavailable);
 assert((await thirdCard.locator('.question-note').textContent()).includes('Delivery is unconfirmed'));
 assert.equal(await thirdCard.locator('textarea').inputValue(),'Keep after native loss');
 assert(await thirdCard.locator('.question-submit').isDisabled());
 // Recovery is explicit, exact-owner addressed and carries no answer payload.
 await page.evaluate(()=>{window.__recoveries=[];const before=window.fetch;window.fetch=async(url,opts)=>{
  if(String(url).endsWith('/questions/reconnect')) {window.__recoveries.push(JSON.parse(opts.body));return {ok:true,status:202,json:async()=>({state:'connecting',message:'Reconnecting. No answer was submitted.'})};}
  return before(url,opts);
 };});
 const answerCount=await page.evaluate(()=>window.__answers.length);
 await thirdCard.locator('.question-reconnect').click();
 await page.waitForFunction(()=>window.__recoveries.length===1);
 assert.deepEqual(await page.evaluate(()=>window.__recoveries[0]),{agent:'codex',session:'s1',owner:'owner:1'});
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[{...third,delivery:'submitted'}]});
 assert(await thirdCard.locator('.question-submit').isDisabled());
 assert.equal(await thirdCard.locator('textarea').inputValue(),'Keep after native loss');
 assert.equal(await page.evaluate(()=>window.__answers.length),answerCount,'recovery never sends an answer');
 // Unsent drafts become editable again; other viewers see a sent replay locked.
 const fourth={...third,id:'"fourth"',turn:'turn-four',item:'fourth-call'};
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[fourth]});
 const fourthCard=page.locator('.question-card').last();await fourthCard.locator('textarea').fill('Unsent recovery draft');
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,available:false,pending:[fourth],reason:'Socket closed'});
 assert(await fourthCard.locator('.question-submit').isDisabled());
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,pending:[fourth]});
 assert(await fourthCard.locator('.question-submit').isEnabled());
 assert.equal(await fourthCard.locator('textarea').inputValue(),'Unsent recovery draft');
 // Session clear invalidates old drafts; a late reply cannot operate on a new agent.
 const changed=structuredClone(status);changed.facts.agents[0].session='rebound';
 await page.evaluate(d=>window.__send('status',d),changed);
 assert.equal(await page.locator('.question-submit:not([disabled])').count(),0);
 await page.evaluate(d=>window.__send('questions',{agents:{codex:d}}),{...window,session:'rebound',pending:[],available:false,reason:'Socket closed'});
 assert(await page.locator('.native-reconnect:visible').isEnabled(),'recovery remains available with no pending question');
 await page.locator('.native-reconnect:visible').click();
 await page.waitForFunction(()=>window.__recoveries.length===2);
 assert.equal(await page.evaluate(()=>window.__recoveries[1].session),'rebound');
 // Claude native completion is the correlated winner receipt. A late HTTP
 // Submitted response must not downgrade an already confirmed local answer.
 const claudeRequest={...request,id:'"tool-claude"',turn:'claude-prompt',item:'tool-claude',blocking:true,
  question:{provider:'claude',fields:[{...fields[0],id:'0'}]}};
 const claudeWindow={session:'claude-session',owner:'claude:owner',available:true,pending:[claudeRequest],ended:null};
 await page.evaluate(d=>window.__send('questions',{agents:{claude:d}}),claudeWindow);
 await page.locator('#roster [data-label=claude]').click();await page.locator('#mode-transcript').click();
 const claudeCard=page.locator('.question-card:visible');assert((await claudeCard.textContent()).includes('Claude question'));
 await claudeCard.locator('input[type=radio]').nth(1).check();await page.evaluate(()=>window.__answerMode='hold');
 await claudeCard.locator('.question-submit').click();
 await page.evaluate(d=>window.__send('questions',{agents:{claude:d}}),{...claudeWindow,pending:[{...claudeRequest,delivery:'local'}]});
 await page.evaluate(()=>window.__releaseAnswer());
 assert((await claudeCard.locator('.question-note').textContent()).includes('Answered in Claude.'));
 assert(await claudeCard.locator('.question-submit').isDisabled());
 await page.evaluate(d=>window.__send('questions',{agents:{claude:d}}),{...claudeWindow,pending:[{...claudeRequest,delivery:'answered'}]});
 assert((await claudeCard.locator('.question-note').textContent()).includes('Claude confirmed'));
 assert(await claudeCard.locator('.question-submit').isDisabled());
 await page.evaluate(d=>window.__send('questions',{agents:{claude:d}}),{...claudeWindow,pending:[]});
 assert((await claudeCard.locator('.question-note').textContent()).includes('Claude confirmed'),'later prompt retention cannot erase a known receipt');
 assert.deepEqual(errors,[]);
 console.log('PASS: native-only question cards, typed multi/other/private answers, inert strings, opaque IDs, preserved focus/drafts, no duplicate cards/sends, cancellation before HTTP reply, unknown outcome/replay fencing, explicit reconnect with no answer dispatch, recovered unsent drafts, empty-owner recovery, Claude completion before late HTTP replies, session clear, private storage and mobile widths.');
}finally{await done();}})().catch(error=>{console.error(error);process.exitCode=1;});
