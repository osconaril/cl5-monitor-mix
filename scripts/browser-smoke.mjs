#!/usr/bin/env node
// Runs against a real Chromium instance so phone layout regressions are caught
// without adding a JavaScript package dependency to this Rust workspace.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createConnection, createServer } from 'node:net';

const root = new URL('..', import.meta.url).pathname;
const temp = await mkdtemp(join(tmpdir(), 'cl5-browser-'));
async function unusedPort() {
  const listener = createServer();
  listener.listen(0, '127.0.0.1');
  await once(listener, 'listening');
  const port = listener.address().port;
  await new Promise(resolve => listener.close(resolve));
  return port;
}
const port = await unusedPort();
const consolePort = await unusedPort();
const debugPort = await unusedPort();
const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const webDist = join(temp, 'web');
let server;
let fake;
let engineer;
let browser;
// Keep the normal end-to-end timings by default.  The fast mode is useful for
// exercising this long smoke suite inside short-lived CI shells; polling still
// happens, just without spending hundreds of milliseconds between successful
// local readbacks.
const sleep = ms => new Promise(resolve => setTimeout(resolve, process.env.CL5_SMOKE_FAST ? Math.min(ms, 40) : ms));
async function buildWeb() {
  // launchd sets NO_COLOR=1, while Trunk accepts only boolean spellings.
  const build = spawn('env', ['-u', 'NO_COLOR', 'trunk', 'build', '--dist', webDist], { cwd: join(root, 'crates', 'web'), stdio: 'inherit' });
  const code = await new Promise((resolve, reject) => {
    build.once('error', reject);
    build.once('exit', resolve);
  });
  if (code !== 0) throw new Error(`trunk build failed with exit ${code}`);
}
async function ready(url) { for (let i = 0; i < 100; i++) { try { if ((await fetch(url)).ok) return; } catch {} await new Promise(resolve => setTimeout(resolve, 100)); } throw new Error(`Timed out waiting for ${url}`); }
async function command(ws, method, params = {}) { const id = ++command.id; ws.send(JSON.stringify({ id, method, params })); return new Promise((resolve, reject) => { const timer = setTimeout(() => reject(new Error(`${method} timed out`)), 10000); const listener = event => { const message = JSON.parse(event.data); if (message.id === id) { clearTimeout(timer); ws.removeEventListener('message', listener); message.error ? reject(new Error(message.error.message)) : resolve(message.result); } }; ws.addEventListener('message', listener); }); }
command.id = 0;
async function stop(process) {
  if (!process) return;
  if (process.exitCode !== null) return;
  const exited = once(process, 'exit');
  process.kill();
  await exited;
}
try {
  await buildWeb();
  server = spawn('cargo', ['run', '-p', 'cl5-server'], {
    cwd: root,
    env: { ...process.env, CL5_LISTEN: `127.0.0.1:${port}`, CL5_DATABASE_PATH: join(temp, 'test.sqlite'), CL5_WEB_DIST: webDist, CL5_CONSOLE_ADDRESS:`127.0.0.1:${consolePort}` },
    stdio: 'ignore',
  });
  browser = spawn(chrome, ['--headless=new', '--no-first-run', '--no-default-browser-check', `--remote-debugging-port=${debugPort}`, `--user-data-dir=${join(temp, 'chrome')}`], { stdio: 'ignore' });
  await ready(`http://127.0.0.1:${port}/setup`);
  await ready(`http://127.0.0.1:${debugPort}/json/version`);
  const target = await fetch(`http://127.0.0.1:${debugPort}/json/new?http://127.0.0.1:${port}/setup`, { method: 'PUT' }).then(r => r.json());
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
  await command(ws, 'Page.enable');
  await command(ws, 'Runtime.evaluate', { expression: "fetch('/setup', {method: 'POST', headers: {'content-type': 'application/x-www-form-urlencoded'}, body: 'name=Browser&passcode=1234'}).then(() => fetch('/signout', {method: 'POST'}))", awaitPromise: true });
  await command(ws, 'Page.navigate', { url: `http://127.0.0.1:${port}/` });
  await sleep(200);
  for (const [width, height] of [[390, 844], [844, 390]]) {
    await command(ws, 'Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: true });
    await command(ws, 'Page.navigate', { url: `http://127.0.0.1:${port}/` });
    await sleep(200);
    const result = await command(ws, 'Runtime.evaluate', { expression: 'JSON.stringify({scroll: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), width: innerWidth, buttons: [...document.querySelectorAll("button")].map(b => b.getBoundingClientRect().height), picker: document.querySelectorAll(".name-picker button").length, text: document.body.innerText})', returnByValue: true });
    const page = JSON.parse(result.result.value);
    if (page.scroll > page.width || page.buttons.some(height => height < 44) || page.picker !== 1 || !page.text.includes('Tap your name')) throw new Error(`Sign-in phone layout failed at ${width}x${height}: ${JSON.stringify(page)}`);
  }
  await command(ws, 'Runtime.evaluate', { expression: "fetch('/signin', {method: 'POST', headers: {'content-type': 'application/x-www-form-urlencoded'}, body: 'name=Browser&passcode=1234'})", awaitPromise: true });
  await command(ws, 'Runtime.evaluate', { expression: "fetch('/api/admin/users', {method: 'POST', headers: {'content-type': 'application/x-www-form-urlencoded'}, body: 'name=Delete+Me&role=performer&passcode=1234'})", awaitPromise: true });
  await command(ws, 'Page.navigate', { url: `http://127.0.0.1:${port}/` });
  await sleep(250);
  const deleteAction = action => `(() => { const card = [...document.querySelectorAll('.user-card')].find(card => card.querySelector('h3')?.textContent === 'Delete Me'); const button = [...(card?.querySelectorAll('button') ?? [])].find(button => button.textContent === '${action}'); if (!button) return false; button.click(); return true; })()`;
  const requestedDelete = await command(ws, 'Runtime.evaluate', { expression: deleteAction('Delete user'), returnByValue: true });
  if (!requestedDelete.result.value) throw new Error('Delete confirmation test user or initial action was missing.');
  await sleep(50);
  const confirmation = await command(ws, 'Runtime.evaluate', { expression: "document.body.innerText.includes('Delete Delete Me? Their saved settings go too.') && document.body.innerText.includes('Confirm delete')", returnByValue: true });
  if (!confirmation.result.value) throw new Error('Delete confirmation did not appear.');
  const cancelledDelete = await command(ws, 'Runtime.evaluate', { expression: deleteAction('Cancel'), returnByValue: true });
  if (!cancelledDelete.result.value) throw new Error('Delete cancellation control was missing.');
  await sleep(50);
  const cancellation = await command(ws, 'Runtime.evaluate', { expression: "!document.body.innerText.includes('Confirm delete') && document.body.innerText.includes('Delete Me')", returnByValue: true });
  if (!cancellation.result.value) throw new Error('Delete cancellation did not preserve the user.');
  await command(ws, 'Runtime.evaluate', { expression: deleteAction('Delete user'), returnByValue: true });
  await sleep(50);
  const confirmedDelete = await command(ws, 'Runtime.evaluate', { expression: `(() => { const card = [...document.querySelectorAll('.user-card')].find(card => card.querySelector('h3')?.textContent === 'Delete Me'); const button = [...(card?.querySelectorAll('button') ?? [])].find(button => button.textContent === 'Confirm delete'); if (!button) return false; button.form.requestSubmit(button); return true; })()`, returnByValue: true });
  if (!confirmedDelete.result.value) throw new Error('Delete confirmation control was missing.');
  await sleep(50);
  for (const [width, height] of [[390, 844], [844, 390]]) {
    await command(ws, 'Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: true });
    await command(ws, 'Page.navigate', { url: `http://127.0.0.1:${port}/` });
    await sleep(250);
    const admin = await command(ws, 'Runtime.evaluate', { expression: 'JSON.stringify({scroll: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), width: innerWidth, text: document.body.innerText})', returnByValue: true });
    const page = JSON.parse(admin.result.value);
    if (page.scroll > page.width || !page.text.includes('Users') || !page.text.includes('Console disconnected') || !page.text.includes('Browser')) throw new Error(`Admin phone layout failed at ${width}x${height}: ${JSON.stringify(page)}`);
  }

  // Performer controls exercise the real HTTP/WebSocket bridge and fake RCP
  // console, never a production host or database.
  await command(ws, 'Runtime.evaluate', { expression: "(async () => { await fetch('/api/admin/users', {method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Singer&role=performer&passcode=5678'}); await fetch('/api/admin/users/Singer/mixes', {method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'mixes=1&mixes=2'}); await fetch('/signout',{method:'POST'}); await fetch('/signin',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Singer&passcode=5678'}); })()", awaitPromise: true });
  fake = spawn('cargo', ['run', '-p', 'fake-console'], {
    cwd: root, env: {...process.env, FAKE_CONSOLE_ADDR:`127.0.0.1:${consolePort}`}, stdio: 'ignore'
  });
  for (let i = 0; i < 150; i++) {
    try {
      engineer = createConnection({host:'127.0.0.1', port:consolePort});
      await once(engineer, 'connect');
      break;
    } catch { engineer.destroy(); engineer = null; await sleep(100); }
  }
  if (!engineer) throw new Error('Fake console failed to start');
  // Drain notifications; readbacks below each use a bounded listener.
  engineer.on('data', () => {});
  async function rcp(line, expected) {
    let buffer = '';
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { engineer.off('data', listener); reject(new Error('RCP timeout: '+line)); }, 5000);
      const listener = chunk => {
        buffer += chunk.toString();
        if (buffer.split('\n').some(line => line.trim() === expected)) {
          clearTimeout(timer); engineer.off('data', listener); resolve();
        }
      };
      engineer.on('data', listener);
      engineer.write(line+'\n');
    });
  }
  async function evaluate(expression) {
    const result = await command(ws, 'Runtime.evaluate', {expression, returnByValue:true, awaitPromise:true});
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  async function expectBrowser(expression, label) {
    for (let i=0; i<300; i++) { if (await evaluate(expression)) return; await sleep(100); }
    throw new Error(label + ": " + await evaluate("JSON.stringify({title:document.title,text:document.body.innerText.slice(0,2000)})"));
  }
  await rcp('set MIXER:Current/InCh/Label/Name 0 0 "Lead Vocal"', 'OK');
  await rcp('set MIXER:Current/InCh/ToMix/Level 0 0 -350', 'OK');
  await rcp('set MIXER:Current/InCh/ToMix/On 0 0 1', 'OK');
  await command(ws, 'Page.navigate', {url:`http://127.0.0.1:${port}/`});
  await expectBrowser("document.querySelectorAll('.mix-picker button').length === 2 && document.title === 'Soundcheck · Monitor Mix'", 'Assigned-mix picker or live scene missing');
  await evaluate("document.querySelector('.mix-picker button').click()");
  const strip = "document.querySelector('.channel-strip')";
  await expectBrowser(`document.querySelectorAll('.channel-strip').length === 80 && ${strip}?.innerText.includes('-3.5 dB') && ${strip}?.innerText.includes('Lead Vocal')`, 'Console fractional level/name was not rendered');
  await expectBrowser(`${strip}.querySelector('.channel-color').getAttribute('aria-label')==='Channel color: Blue'`, 'Initial console color missing');
  await rcp('set MIXER:Current/InCh/Label/Color 0 0 "Red"', 'OK');
  await expectBrowser(`${strip}.querySelector('.channel-color').getAttribute('aria-label')==='Channel color: Red' && getComputedStyle(${strip}.querySelector('.channel-color')).backgroundColor==='rgb(244, 67, 54)'`, 'Live console color missing');
  await rcp('set MIXER:Current/InCh/Label/Color 0 0 "Off"', 'OK');
  await expectBrowser(`${strip}.querySelector('.channel-color').getAttribute('aria-label')==='Channel color: Off' && getComputedStyle(${strip}.querySelector('.channel-color')).backgroundColor==='rgba(0, 0, 0, 0)'`, 'Off color was not cleared');
  if (!await evaluate("document.querySelectorAll('.channel-panel input[type=range]').length===0")) throw new Error('Mono mix offered pan');
  await rcp('set MIXER:Current/Mix/Role 0 0 "StereoL"', 'OK');
  await evaluate(`${strip}.querySelector('.channel-name').click()`);
  await expectBrowser("document.querySelectorAll('.channel-panel input[aria-label$=pan]').length===1", 'Stereo role did not show pan in the channel panel');
  await rcp('set MIXER:Current/InCh/ToMix/Pan 0 0 -17', 'OK');
  await expectBrowser("document.querySelector('.channel-panel').innerText.includes('Left or right in your ears')", 'Stereo channel panel missing');
  await evaluate("[...document.querySelectorAll('.channel-panel button')].find(button => button.textContent === 'Done').click()");
  // The performer shell owns the viewport: the document never scrolls, while
  // its channel rail does. At each promised viewport, every control on a
  // visible strip must be reachable rather than clipped below the page edge.
  for (const [width,height] of [[390,844],[844,390],[1280,720],[1440,900]]) {
    await command(ws, 'Emulation.setDeviceMetricsOverride', {width,height,deviceScaleFactor:1,mobile:true});
    if (!await evaluate(`(() => {
      const visible = [...document.querySelectorAll('.channel-strip')].filter(strip => {
        const r = strip.getBoundingClientRect();
        return r.right > 0 && r.left < innerWidth && r.bottom > 0 && r.top < innerHeight;
      });
      const controlsFit = visible.every(strip => [...strip.querySelectorAll('.channel-name, input, button')]
        .every(control => { const r = control.getBoundingClientRect(); return r.left >= 0 && r.right <= innerWidth && r.top >= 0 && r.bottom <= innerHeight; }));
      return Math.max(document.documentElement.scrollWidth, document.body.scrollWidth) <= innerWidth
        && document.scrollingElement.scrollHeight <= innerHeight
        && [...document.querySelectorAll('button')].every(b=>b.getBoundingClientRect().height>=44)
        && controlsFit
        && !document.body.innerText.toLowerCase().includes('wedge');
    })()`))
      throw new Error('Performer page overflow, clipped visible controls, small controls or incorrect wording');
    await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -350');
  }
  // Real touch input: a swipe begun on a fader scrolls without changing sound;
  // holding first arms it. Orientation cancels a held gesture and retains the
  // first visible strip. Count every browser control request during these steps.
  await command(ws, 'Emulation.setTouchEmulationEnabled', {enabled:true, maxTouchPoints:1});
  await evaluate(`(() => {
    window.controlWrites = [];
    const original = window.fetch;
    window.fetch = function(input, init) {
      const url = typeof input === 'string' ? input : input.url;
      if (url.includes('/api/console') || url.includes('/api/performer/solo') || url.includes('/api/performer/group'))
        window.controlWrites.push(url);
      return original.apply(this, arguments);
    };
  })()`);
  const touch = (type, x, y) => command(ws, 'Input.dispatchTouchEvent', {
    type, touchPoints: type === 'touchEnd' ? [] : [{x,y}]
  });
  // Chromium's CDP touch injector can hit-test a range input as BODY in this
  // headless configuration. Deliver range gestures to their measured control
  // instead; the production handler is delegated from document and only relies
  // on PointerEvent fields.
  const pointerOn = (selector, type, x, y) => evaluate(`(() => {
    const target = document.querySelector(${JSON.stringify(selector)});
    if (!target) throw new Error('Missing touch target: ${selector}');
    target.dispatchEvent(new PointerEvent(${JSON.stringify(type)}, {
      bubbles: true, cancelable: true, pointerId: 71, isPrimary: true,
      pointerType: 'touch', button: 0, clientX: ${x}, clientY: ${y}
    }));
  })()`);
  for (const [width,height] of Array(3).fill([[390,844],[844,390]]).flat()) {
    await command(ws,'Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:true});
    await evaluate("new Promise(requestAnimationFrame)");
    await evaluate("window.dispatchEvent(new Event('resize'))");
    await sleep(150);
    await evaluate("document.querySelector('.channel-strips').scrollIntoView({block:'center'})");
    const geometry = await evaluate(`(() => {
      const a=document.querySelector('.channel-strips'), strips=a.children;
      const first=strips[0].getBoundingClientRect(), second=strips[1].getBoundingClientRect();
      const f=strips[0].querySelector('input').getBoundingClientRect();
      return {rows:second.top>first.top, columns:second.left>first.left,
        vertical:getComputedStyle(strips[0].querySelector('input')).writingMode==='vertical-lr',
        x:f.x+f.width/2, y:f.y+f.height/2,
        scene:document.querySelector('.scene-bar').getBoundingClientRect().height};
    })()`);
    if (width<height ? !geometry.rows || geometry.vertical : !geometry.columns || !geometry.vertical || geometry.scene>40)
      throw new Error('Wrong fader orientation or scene bar: '+JSON.stringify(geometry));
    await pointerOn('.channel-strip input', 'pointerdown', geometry.x, geometry.y);
    await pointerOn('.channel-strip input', 'pointermove', geometry.x-(width>height?60:0), geometry.y-(width<height?60:0));
    await pointerOn('.channel-strip input', 'pointerup', geometry.x-(width>height?60:0), geometry.y-(width<height?60:0));
    if (!await evaluate(`document.querySelector('.channel-strips').${width>height?'scrollLeft':'scrollTop'}>0 && window.controlWrites.length===0`))
      throw new Error('A swipe on a fader failed to scroll or changed sound at '+width+' '+JSON.stringify(geometry)+' '+await evaluate("JSON.stringify({scroll:document.querySelector('.channel-strips').scrollTop,left:document.querySelector('.channel-strips').scrollLeft,writes:window.controlWrites,scripts:[...document.scripts].map(s=>s.src)})"));
    await evaluate(`(() => {
      const a=document.querySelector('.channel-strips'), c=a.querySelector('[data-channel="40"]');
      a.scrollLeft = ${width>height?'c.offsetLeft':'0'};
      a.scrollTop = ${width<height?'c.offsetTop':'0'};
    })()`);
    await evaluate("new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))");
    // Let the rail's scroll listener record the leading strip before the
    // viewport mutation below; browsers deliver programmatic scroll events
    // after the animation-frame callbacks above.
    await sleep(100);
    const leadingChannel = await evaluate(`(() => { const a=document.querySelector('.channel-strips'), b=a.getBoundingClientRect(); return [...a.children].find(c => { const r=c.getBoundingClientRect(); return ${width > height ? 'r.right > b.left + 1' : 'r.bottom > b.top + 1'}; })?.dataset.channel; })()`);
    await command(ws,'Emulation.setDeviceMetricsOverride',{width:height,height:width,deviceScaleFactor:1,mobile:true});
    await evaluate("new Promise(requestAnimationFrame)");
    await evaluate("window.dispatchEvent(new Event('resize'))");
    await sleep(150);
    if (!await evaluate(`(() => {const a=document.querySelector('.channel-strips'), b=a.getBoundingClientRect(), r=a.querySelector('[data-channel="${leadingChannel}"]').getBoundingClientRect(); return ${width<height?'Math.abs(r.left-b.left)<2':'Math.abs(r.top-b.top)<2'} && window.controlWrites.length===0;})()`))
      throw new Error('Rotation moved the visible channel or submitted a control request at '+width+' expected '+leadingChannel+' '+await evaluate("JSON.stringify({writes:window.controlWrites,area:document.querySelector('.channel-strips').getBoundingClientRect().toJSON(),strip:document.querySelector('[data-channel=\"40\"]').getBoundingClientRect().toJSON(),scroll:document.querySelector('.channel-strips').scrollTop,left:document.querySelector('.channel-strips').scrollLeft})"));
    await evaluate("document.querySelector('.channel-strips').scrollTo(0,0)");
    await sleep(100);
  }
  await command(ws,'Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
  await sleep(150);
  await evaluate("document.querySelector('.channel-strips').scrollIntoView({block:'center'})");
  const faderPoint = await evaluate(`(() => {const r=${strip}.querySelector('input').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()`);
  await pointerOn('.channel-strip input', 'pointerdown', faderPoint.x, faderPoint.y);
  await sleep(260);
  await pointerOn('.channel-strip input', 'pointermove', faderPoint.x+10, faderPoint.y);
  await pointerOn('.channel-strip input', 'pointermove', faderPoint.x+11, faderPoint.y);
  await expectBrowser("window.controlWrites.length===1", 'A deliberate hold and move did not adjust the fader');
  await command(ws,'Emulation.setDeviceMetricsOverride',{width:844,height:390,deviceScaleFactor:1,mobile:true});
  await evaluate("new Promise(requestAnimationFrame)");
  await evaluate("window.dispatchEvent(new Event('resize'))");
  await sleep(150);
  await pointerOn('.channel-strip input', 'pointermove', faderPoint.x+20, faderPoint.y);
  await pointerOn('.channel-strip input', 'pointerup', faderPoint.x+20, faderPoint.y);
  if (!await evaluate("window.controlWrites.length===1 && !document.querySelector('[data-adjusting]')"))
    throw new Error('Rotation did not cancel the active fader gesture');
  await rcp('set MIXER:Current/InCh/ToMix/Level 0 0 -350', 'OK');
  await expectBrowser(`${strip}.innerText.includes('-3.5 dB')`, 'Touch test reset did not reach browser');
  await evaluate(`(() => {const f = ${strip}.querySelector('input'); f.value='-3.49'; f.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await expectBrowser(`${strip}.innerText.includes('-3 dB')`, 'Upward fractional move did not snap to -3 dB');
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -300');
  await rcp('set MIXER:Current/InCh/ToMix/Level 0 0 -350', 'OK');
  await expectBrowser(`${strip}.innerText.includes('-3.5 dB')`, 'Engineer changes did not reach open screen');
  await evaluate(`(() => {const f = ${strip}.querySelector('input'); f.value='-3.51'; f.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await expectBrowser(`${strip}.innerText.includes('-4 dB')`, 'Downward fractional move did not snap to -4 dB');
  await evaluate(`${strip}.querySelector('[data-action="mute"]').click()`);
  await expectBrowser(`${strip}.querySelector('[data-action="mute"]').getAttribute('aria-pressed') === 'true'`, 'Mute readback did not reach the browser');
  await rcp('get MIXER:Current/InCh/ToMix/On 0 0', 'OK get MIXER:Current/InCh/ToMix/On 0 0 0');
  await evaluate("[...document.querySelectorAll('button')].find(button => button.textContent === 'Arrange').click()");
  await expectBrowser("document.querySelector('.reorder-handle')", 'Arrange mode did not expose reorder controls');
  async function dragStrip(source, target, after=false) {
    const points = await evaluate(`(() => {
      const s=document.querySelector('[data-channel="${source}"] .reorder-handle').getBoundingClientRect();
      const t=document.querySelector('[data-channel="${target}"]').getBoundingClientRect();
      return {sx:s.x+s.width/2,sy:s.y+s.height/2,tx:t.x+t.width*(${after}?0.75:0.25),ty:matchMedia("(orientation: landscape)").matches?t.y+22:t.y+t.height*(${after}?0.75:0.1)};
    })()`);
    // Chromium's headless CDP touch injector can hit-test controls as BODY.
    // Dispatch the measured touch-pointer sequence to the handle instead;
    // production pointers retain capture, while the handler accepts this
    // synthetic sequence without attempting an invalid capture.
    const handle = `[data-channel="${source}"] .reorder-handle`;
    await pointerOn(handle, 'pointerdown', points.sx, points.sy);
    await pointerOn(handle, 'pointermove', points.tx, points.ty);
    await pointerOn(handle, 'pointerup', points.tx, points.ty);
  }
  const writesBeforeReorder = await evaluate("window.controlWrites.length");
  for (const [width,height] of [[390,844],[844,390]]) {
    await command(ws,'Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:true});
    await sleep(150);
    await evaluate("document.querySelector('.channel-strips').scrollTo(0,0); document.querySelector('.channel-strips').scrollIntoView({block:'center'})");
    await dragStrip(1,0);
    await expectBrowser("document.querySelector('.channel-strip:not(.group-strip)').dataset.channel==='1' && !document.querySelector('.reorder-handle').disabled", 'Touch drag did not reorder actual strips');
    if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.order[0]===1 && l.order[1]===0)"))
      throw new Error('Touch drag order was not persisted');
    await dragStrip(0,1);
    await expectBrowser("document.querySelector('.channel-strip').dataset.channel==='0' && !document.querySelector('.reorder-handle').disabled", 'Touch drag did not restore strip order');
  }
  // A refused reorder must not present a new order as saved.
  await evaluate(`(() => {
    const original=window.fetch;
    let refuse=true;
    window.fetch=function(input,init) {
      const url=typeof input==='string'?input:input.url;
      if (refuse && url.includes('/api/performer/layout') && (init?.method??input.method)==='PUT') {
        refuse=false;
        return Promise.resolve(new Response('Channel reorder was refused.',{status:403}));
      }
      return original.apply(this,arguments);
    };
  })()`);
  await dragStrip(1,0);
  await expectBrowser("document.body.innerText.includes('Channel reorder was refused.') && !document.querySelector('.reorder-handle').disabled", 'Refused reorder did not show an error');
  if (!await evaluate(`document.querySelector('.channel-strip').dataset.channel==='0' && window.controlWrites.length===${writesBeforeReorder}`))
    throw new Error('Reorder changed sound or a refused reorder changed the displayed order');
  // Presentation settings must survive a fresh session without changing sound.
  const clickText = text => evaluate(`(() => { const b=[...document.querySelectorAll('button')].find(b=>b.textContent===${JSON.stringify(text)}); if(!b) throw new Error('Missing button'); b.click(); })()`);
  await rcp('set MIXER:Current/Mix/Role 0 0 "Mono"', 'OK');
  await expectBrowser("document.querySelectorAll('.pan-control').length===0", 'Unlink did not hide pan');
  const monoRefusal=await evaluate("fetch('/api/console',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({path:'MIXER:Current/InCh/ToMix/Pan',input:0,mix:0,value:'12'})}).then(async r=>!r.ok && (await r.text()).includes('stereo-linked'))");
  if (!monoRefusal) throw new Error('Server accepted pan on mono mix');
  // Arrange replaces the legacy layout page. It saves each drag/hide directly,
  // keeps groups in the layout data without rendering group controls, and never
  // changes console values merely because a strip is hidden.
  await expectBrowser("document.querySelector('.reorder-handle') && document.body.innerText.includes('Hidden. You still hear these in your mix.')", 'Arrange mode did not replace the legacy layout editor');
  await evaluate("document.querySelector('[data-channel=\"2\"] [data-action=\"hide\"]').click()");
  await expectBrowser("!document.querySelector('[data-channel=\"2\"]') && document.querySelector('.hidden-channels')", 'Hide did not move a strip to the hidden tray');
  await rcp('get MIXER:Current/InCh/ToMix/Level 2 0', 'OK get MIXER:Current/InCh/ToMix/Level 2 0 -350');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.hidden.includes(2))"))
    throw new Error('Hide was not persisted');
  await clickText('Show Channel 3');
  await expectBrowser("document.querySelector('[data-channel=\"2\"]')", 'Show did not restore the hidden strip');
  if (!await evaluate(`(async()=>{const l=await fetch('/api/performer/layout').then(r=>r.json());l.groups=[{id:'saved-group',name:'Band',channels:[0,1]}];const r=await fetch('/api/performer/layout',{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify(l)});return r.ok;})()`))
    throw new Error('Could not seed a saved group for the performer persistence check');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.groups[0]?.name==='Band')"))
    throw new Error('Saved layout groups were not retained');
  await clickText('Done');
  await expectBrowser("!document.querySelector('.reorder-handle') && !document.querySelector('.group-strip') && !document.body.innerText.includes('Edit channel layout')", 'Arrange mode left legacy performer controls visible');
  // Hold only the assignment refresh so actual controls can hit server-side
  // revocation before the UI learns of it. No refusal response is fabricated.
  await evaluate(`(async()=>{const assignments=await fetch('/api/performer/mixes').then(r=>r.json());window.assignmentFetch=window.fetch;window.fetch=function(input,init){const url=typeof input==='string'?input:input.url;if(url.includes('/api/performer/mixes'))return Promise.resolve(new Response(JSON.stringify(assignments),{headers:{'content-type':'application/json'}}));return window.assignmentFetch.apply(this,arguments);};})()`);
  const adminLogin=await fetch(`http://127.0.0.1:${port}/signin`,{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Browser&passcode=1234',redirect:'manual'});
  const adminCookie=adminLogin.headers.get('set-cookie')?.split(';')[0];
  if (!adminCookie) throw new Error('Admin test session missing');
  async function assignSinger(body) {
    const response=await fetch(`http://127.0.0.1:${port}/api/admin/users/Singer/mixes`,{method:'POST',headers:{cookie:adminCookie,'content-type':'application/x-www-form-urlencoded'},body,redirect:'manual'});
    if (response.status!==303) throw new Error('Assignment update failed: '+response.status);
  }
  await rcp('set MIXER:Current/Mix/Role 0 0 "StereoL"', 'OK');
  await evaluate("document.querySelector('[data-channel=\"0\"] .channel-name').click()");
  await expectBrowser("document.querySelectorAll('.channel-panel input[aria-label$=pan]').length===1", 'Stereo pan missing from the channel panel before revocation');
  await evaluate(`(()=>{window.refusalResults=[];const original=window.fetch;window.fetch=async function(input,init){const response=await original.apply(this,arguments);const url=typeof input==='string'?input:input.url;if(init?.method==='POST' || input?.method==='POST')window.refusalResults.push({url,status:response.status,body:await response.clone().text()});return response;};})()`);
  await assignSinger('mixes=2');
  for (const action of [
    `document.querySelector('[data-channel="0"] [data-action="mute"]').click()`,
    `document.querySelector('[data-channel="0"] [data-action="solo"]').click()`,
    `(()=>{const f=document.querySelector('[data-channel="0"] .send-control input');f.value='-3';f.dispatchEvent(new Event('input',{bubbles:true}));})()`,
    `(()=>{const f=document.querySelector('.channel-panel input[aria-label$=pan]');f.value='12';f.dispatchEvent(new Event('input',{bubbles:true}));})()`
  ]) {
    const count=await evaluate('window.refusalResults.length');
    await evaluate(action);
    await expectBrowser(`window.refusalResults.length===${count+1} && window.refusalResults.at(-1).status===403 && window.refusalResults.at(-1).body.includes('not assigned')`, 'Revoked action did not receive actual server refusal');
    await expectBrowser("document.querySelector('.performer').innerText.includes('not assigned')", 'Actual assignment refusal was not shown');
    await sleep(100);
  }
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -150');
  await rcp('get MIXER:Current/InCh/ToMix/On 0 0', 'OK get MIXER:Current/InCh/ToMix/On 0 0 0');
  await evaluate('window.fetch=window.assignmentFetch');
  await expectBrowser("!document.querySelector('.channel-strip') && document.body.innerText.includes('no longer assigned')", 'Revocation silently switched the performer to another mix');
  await assignSinger('mixes=1&mixes=2');
  await expectBrowser("document.querySelectorAll('.channel-strip').length===80", 'Restored assignment did not return controls');
  await rcp('set MIXER:Current/Mix/Role 0 0 "Mono"', 'OK');

  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0','OK get MIXER:Current/InCh/ToMix/Level 0 0 -150');
  await rcp('set MIXER:Current/InCh/ToMix/Level 0 0 -400','OK');
  await evaluate("fetch('/signout',{method:'POST'}).then(()=>fetch('/signin',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Singer&passcode=5678'}))");
  await command(ws,'Page.navigate',{url:`http://127.0.0.1:${port}/`});
  await expectBrowser("document.querySelectorAll('.mix-picker button').length===2", 'New session picker missing');
  await evaluate("document.querySelector('.mix-picker button').click()");
  await expectBrowser("document.querySelectorAll('.channel-strip').length===80 && !document.querySelector('.group-strip')", 'Performer layout did not return without group UI');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.groups[0]?.name==='Band')"))
    throw new Error('Performer reload discarded saved groups');
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -400');
  await rcp('get MIXER:Current/InCh/ToMix/On 0 0', 'OK get MIXER:Current/InCh/ToMix/On 0 0 0');
  // Personal solo restores exact wire levels and keeps engineer changes.
  await rcp('set MIXER:Current/InCh/ToMix/Level 2 0 -350', 'OK');
  await expectBrowser("document.querySelector('[data-channel=\"2\"]').innerText.includes('-3.5 dB')", 'Solo baseline did not reach the browser');
  const soloButton = input => `document.querySelector('[data-channel="${input}"]').querySelector('[data-action="solo"]')`;
  await evaluate(`${soloButton(0)}.click()`);
  await expectBrowser("document.querySelector('.solo-banner')?.innerText.includes('Solo: Lead Vocal') && document.querySelector('.solo-banner button')?.textContent==='Clear solo' && document.querySelector('[data-channel=\"2\"]').innerText.includes('Paused')", 'Solo did not show its persistent banner or pause other channels');
  await rcp('get MIXER:Current/InCh/ToMix/Level 2 0', 'OK get MIXER:Current/InCh/ToMix/Level 2 0 -32768');
  await rcp('set MIXER:Current/InCh/ToMix/Level 3 0 -725', 'OK');
  await expectBrowser("document.querySelector('[data-channel=\"3\"]').innerText.includes('-7.25 dB')", 'Engineer change during solo missing');
  await evaluate("document.querySelector('.solo-banner button').click()");
  await expectBrowser("!document.querySelector('.solo-banner') && document.querySelector('[data-channel=\"2\"]').innerText.includes('-3.5 dB')", 'Clear solo did not restore fractional level');
  await rcp('get MIXER:Current/InCh/ToMix/Level 3 0', 'OK get MIXER:Current/InCh/ToMix/Level 3 0 -725');
  await sleep(300); // Let restoration readbacks drain before starting another solo.
  await evaluate(`${soloButton(1)}.click()`);
  await expectBrowser("document.querySelector('.solo-banner')?.innerText.includes('Solo: Channel 2')", 'Second solo did not start');
  await rcp('ssrecall_ex MIXER:Lib/Scene 2', 'OK');
  await expectBrowser("document.title === 'Show · Monitor Mix' && document.querySelector('.scene-bar').innerText.includes('Show')", 'Scene recall did not update mix page/title');
  await expectBrowser("document.body.innerText.includes('New scene, so Solo turned off.') && !document.querySelector('.solo-banner')", 'Scene recall did not clear solo and explain why');
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -1200');
  await evaluate("[...document.querySelectorAll('button')].find(b=>b.textContent==='Choose another mix').click()");
  await expectBrowser("document.querySelectorAll('.mix-picker button').length === 2 && document.title === 'Show · Monitor Mix'", 'Picker lost current scene');
  await rcp('ssrecall_ex MIXER:Lib/Scene 3', 'OK');
  await expectBrowser("document.title === 'Scene 3 · Monitor Mix' && document.querySelector('.scene-bar').innerText.includes('Scene 3')", 'Unnamed scene missing on picker');
  await evaluate("document.querySelector('.mix-picker button').click()");
  await expectBrowser("document.querySelectorAll('.channel-strip').length===80 && document.title==='Scene 3 · Monitor Mix' && document.querySelector('.scene-bar').innerText.includes('Scene 3')", 'Unnamed scene missing on mix screen');
  await evaluate("[...document.querySelectorAll('button')].find(b=>b.textContent==='Choose another mix').click()");
  // Regression: losing the solo owner must still leave an authorized recovery
  // path. Exercise real HTTP authorization, admin UI and TCP console readback.
  async function adminPost(path, body) {
    const response = await fetch(`http://127.0.0.1:${port}${path}`, {
      method:'POST', headers:{cookie:adminCookie,'content-type':'application/x-www-form-urlencoded'},
      body, redirect:'manual'
    });
    if (response.status !== 303) throw new Error(`Admin action failed: ${path} ${response.status}`);
  }
  async function soloRequest(cookie, channels) {
    return fetch(`http://127.0.0.1:${port}/api/performer/solo`, {
      method:'POST', headers:{cookie,'content-type':'application/json'},
      body:JSON.stringify({mix:0,channels})
    });
  }
  await evaluate("fetch('/signout',{method:'POST'}).then(()=>fetch('/signin',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Browser&passcode=1234'}))");
  await command(ws,'Page.navigate',{url:`http://127.0.0.1:${port}/`});
  await expectBrowser("document.querySelector('h1')?.textContent==='Users'", 'Recovery admin portal missing');
  for (const scenario of ['unassigned','deleted','scene']) {
    const name = `Recovery-${scenario}`;
    await adminPost('/api/admin/users', `name=${name}&passcode=9876&role=performer`);
    await adminPost(`/api/admin/users/${name}/mixes`, 'mixes=1');
    const login = await fetch(`http://127.0.0.1:${port}/signin`, {
      method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},
      body:`name=${name}&passcode=9876`,redirect:'manual'
    });
    const cookie = login.headers.get('set-cookie')?.split(';')[0];
    if (!cookie) throw new Error('Recovery performer login failed');
    await expectBrowser("fetch('/api/admin/overview').then(r=>r.json()).then(o=>o.console.connected)", 'Console hydration did not finish before recovery test');
    await rcp('set MIXER:Current/InCh/ToMix/Level 2 0 -350', 'OK');
    await sleep(300);
    const start = await soloRequest(cookie,[0]);
    if (start.status !== 202) throw new Error('Recovery solo start refused: '+await start.text());
    await expectBrowser("document.body.innerText.includes('Clear solo on Mix 1')", 'Admin recovery button missing');
    await rcp('get MIXER:Current/InCh/ToMix/Level 2 0', 'OK get MIXER:Current/InCh/ToMix/Level 2 0 -32768');
    await rcp('set MIXER:Current/InCh/ToMix/Level 3 0 -725', 'OK');
    await sleep(300);
    if (scenario === 'deleted') {
      await adminPost(`/api/admin/users/${name}`, '');
    } else {
      await adminPost(`/api/admin/users/${name}/mixes`, 'mixes=2');
    }
    const refused = await soloRequest(cookie,[]);
    if (refused.status !== (scenario === 'deleted' ? 401 : 403)) throw new Error('Removed owner retained solo authority');
    // An admin may clear, but cannot take over or change another user's selection.
    if ((await soloRequest(adminCookie,[1])).status !== 400) throw new Error('Admin stole active solo');
    if (scenario === 'scene') {
      await rcp('ssrecall_ex MIXER:Lib/Scene 2', 'OK');
      await expectBrowser("!document.body.innerText.includes('Clear solo on Mix 1')", 'Scene left stale admin recovery');
      await expectBrowser("fetch('/api/admin/overview').then(r=>r.json()).then(o=>o.console.connected)", 'Scene recovery hydration did not finish');
      const cleared = await soloRequest(adminCookie,[]);
      if (cleared.status !== 202) throw new Error('Post-scene clear refused');
      await rcp('get MIXER:Current/InCh/ToMix/Level 2 0', 'OK get MIXER:Current/InCh/ToMix/Level 2 0 -32768');
    } else {
      await clickText('Clear solo on Mix 1');
      await expectBrowser("!document.body.innerText.includes('Clear solo on Mix 1')", 'Admin could not recover stranded solo');
      await rcp('get MIXER:Current/InCh/ToMix/Level 2 0', 'OK get MIXER:Current/InCh/ToMix/Level 2 0 -350');
      await rcp('get MIXER:Current/InCh/ToMix/Level 3 0', 'OK get MIXER:Current/InCh/ToMix/Level 3 0 -725');
    }
    await sleep(300);
  }
  await evaluate("fetch('/signout',{method:'POST'}).then(()=>fetch('/signin',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Singer&passcode=5678'}))");
  await command(ws,'Page.navigate',{url:`http://127.0.0.1:${port}/`});
  await expectBrowser("document.querySelectorAll('.mix-picker button').length===2", 'Performer picker missing after recovery checks');
  await stop(fake); fake = null;
  await expectBrowser("document.querySelector('.scene-bar').innerText.includes('Console disconnected — scene unknown')", 'Disconnected console left stale scene name');
  await evaluate("document.querySelector('.mix-picker button').click()");
  await expectBrowser("[...document.querySelectorAll('.channel-strip input, .channel-strip button:not(.reorder-handle)')].every(e=>e.disabled)", 'Disconnected controls remained enabled');
  ws.close();
  console.log('Browser smoke passed at 390x844 and 844x390, including touch-safe faders, rotation, strip drag reorder, performer picker, live controls, saved layouts/groups and scenes.');
} finally {
  engineer?.destroy();
  await Promise.all([stop(server), stop(browser), stop(fake)]);
  await rm(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
