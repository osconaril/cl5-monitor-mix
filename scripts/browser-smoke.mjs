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
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function buildWeb() {
  // launchd sets NO_COLOR=1, while Trunk accepts only boolean spellings.
  const build = spawn('env', ['-u', 'NO_COLOR', 'trunk', 'build', '--dist', webDist], { cwd: join(root, 'crates', 'web'), stdio: 'inherit' });
  const code = await new Promise((resolve, reject) => {
    build.once('error', reject);
    build.once('exit', resolve);
  });
  if (code !== 0) throw new Error(`trunk build failed with exit ${code}`);
}
async function ready(url) { for (let i = 0; i < 100; i++) { try { if ((await fetch(url)).ok) return; } catch {} await sleep(100); } throw new Error(`Timed out waiting for ${url}`); }
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
    const result = await command(ws, 'Runtime.evaluate', { expression: 'JSON.stringify({scroll: Math.max(document.documentElement.scrollWidth, document.body.scrollWidth), width: innerWidth, buttons: [...document.querySelectorAll("button")].map(b => b.getBoundingClientRect().height), picker: document.querySelector("input[name=name]")?.list?.querySelectorAll("option").length, text: document.body.innerText})', returnByValue: true });
    const page = JSON.parse(result.result.value);
    if (page.scroll > page.width || page.buttons.some(height => height < 44) || page.picker !== 1 || !page.text.includes('Sign in')) throw new Error(`Sign-in phone layout failed at ${width}x${height}: ${JSON.stringify(page)}`);
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
  // A resize or readback cannot send a rounded level back to the console.
  for (const [width,height] of [[390,844],[844,390]]) {
    await command(ws, 'Emulation.setDeviceMetricsOverride', {width,height,deviceScaleFactor:1,mobile:true});
    if (!await evaluate("Math.max(document.documentElement.scrollWidth, document.body.scrollWidth) <= innerWidth && [...document.querySelectorAll('button')].every(b=>b.getBoundingClientRect().height>=44) && !document.body.innerText.toLowerCase().includes('wedge')"))
      throw new Error('Performer page overflow, small controls or incorrect wording');
    await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -350');
  }
  await evaluate(`(() => {const f = ${strip}.querySelector('input'); f.value='-3.49'; f.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await expectBrowser(`${strip}.innerText.includes('-3 dB')`, 'Upward fractional move did not snap to -3 dB');
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -300');
  await rcp('set MIXER:Current/InCh/ToMix/Level 0 0 -350', 'OK');
  await expectBrowser(`${strip}.innerText.includes('-3.5 dB')`, 'Engineer changes did not reach open screen');
  await evaluate(`(() => {const f = ${strip}.querySelector('input'); f.value='-3.51'; f.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await expectBrowser(`${strip}.innerText.includes('-4 dB')`, 'Downward fractional move did not snap to -4 dB');
  await evaluate(`${strip}.querySelector('button').click()`);
  await expectBrowser(`${strip}.querySelector('button').getAttribute('aria-pressed') === 'true'`, 'Mute readback did not reach the browser');
  await rcp('get MIXER:Current/InCh/ToMix/On 0 0', 'OK get MIXER:Current/InCh/ToMix/On 0 0 0');
  // Presentation settings must survive a fresh session without changing sound.
  const clickText = text => evaluate(`(() => { const b=[...document.querySelectorAll('button')].find(b=>b.textContent===${JSON.stringify(text)}); if(!b) throw new Error('Missing button'); b.click(); })()`);
  await clickText('Edit channel layout');
  await expectBrowser("document.querySelectorAll('.layout-channel').length===80", 'Layout editor did not load');
  for (const [width,height] of [[390,844],[844,390]]) {
    await command(ws,'Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:true});
    if (!await evaluate("Math.max(document.documentElement.scrollWidth,document.body.scrollWidth)<=innerWidth && [...document.querySelectorAll('.layout-editor button')].every(b=>b.getBoundingClientRect().height>=44)"))
      throw new Error('Layout editor overflow or small touch targets');
  }
  await evaluate(`(() => {
    const source=document.querySelector('[data-layout-channel="1"]');
    const target=document.querySelector('[data-layout-channel="0"]');
    source.dispatchEvent(new DragEvent('dragstart',{bubbles:true}));
    target.dispatchEvent(new DragEvent('drop',{bubbles:true,cancelable:true}));
  })()`);
  await expectBrowser("document.querySelector('.layout-channel').dataset.layoutChannel==='1'", 'Drag reorder did not move channel');
  await evaluate(`[...document.querySelector('[data-layout-channel="2"]').querySelectorAll('button')].find(b=>b.textContent==='Hide channel').click()`);
  await evaluate(`(() => { const name=document.querySelector('[aria-label="New group name"]'); name.value='Drums'; name.dispatchEvent(new Event('input',{bubbles:true})); })()`);
  await evaluate(`document.querySelector('.group-member input[value="0"]').click()`);
  await evaluate(`document.querySelector('.group-member input[value="1"]').click()`);
  await clickText('Add group');
  await expectBrowser("document.querySelector('.layout-group input')?.value==='Drums'", 'Named group was not created');
  await clickText('Save layout');
  await expectBrowser("!document.querySelector('.layout-editor') && document.querySelectorAll('.channel-strip').length===79 && document.querySelector('.channel-strip').dataset.channel==='1'", 'Saved order/visibility was not rendered');
  if (!await evaluate("(async()=>{const l=await fetch('/api/performer/layout').then(r=>r.json()); return l.order[0]===1 && l.hidden.includes(2) && l.groups[0].name==='Drums' && l.groups[0].channels.join(',')==='0,1';})()"))
    throw new Error('Layout persistence readback failed');
  await evaluate("fetch('/signout',{method:'POST'}).then(()=>fetch('/signin',{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:'name=Singer&passcode=5678'}))");
  await command(ws,'Page.navigate',{url:`http://127.0.0.1:${port}/`});
  await expectBrowser("document.querySelectorAll('.mix-picker button').length===2", 'New session picker missing');
  await evaluate("document.querySelector('.mix-picker button').click()");
  await expectBrowser("document.querySelectorAll('.channel-strip').length===79 && document.querySelector('.channel-strip').dataset.channel==='1' && document.querySelector('.hidden-channels')", 'Saved layout did not return on new sign-in');
  await clickText('Edit channel layout');
  await expectBrowser("document.querySelector('.layout-group input')?.value==='Drums'", 'Saved group did not return');
  // A refused save keeps the draft open and leaves persisted data untouched.
  await evaluate("(() => {const n=document.querySelector('.layout-group input'); n.value=''; n.dispatchEvent(new Event('input',{bubbles:true}));})()");
  await clickText('Save layout');
  await expectBrowser("document.querySelector('.layout-editor [role=alert]').textContent.includes('Give each group a name')", 'Refused save has no readable message');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.groups[0].name==='Drums')"))
    throw new Error('Invalid draft replaced saved group');
  await evaluate("(() => {const n=document.querySelector('.layout-group input'); n.value='Band'; n.dispatchEvent(new Event('input',{bubbles:true}));})()");
  await clickText('Save layout');
  await expectBrowser("!document.querySelector('.layout-editor')", 'Group rename did not save');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.groups[0].name==='Band')"))
    throw new Error('Group rename did not persist');
  await clickText('Edit channel layout');
  await expectBrowser("document.querySelector('.layout-group input')?.value==='Band'", 'Renamed group did not load');
  await clickText('Delete group');
  await evaluate(`[...document.querySelector('[data-layout-channel="2"]').querySelectorAll('button')].find(b=>b.textContent==='Show channel').click()`);
  await clickText('Save layout');
  await expectBrowser("!document.querySelector('.layout-editor') && document.querySelectorAll('.channel-strip').length===80", 'Show channel did not restore strip');
  if (!await evaluate("fetch('/api/performer/layout').then(r=>r.json()).then(l=>l.groups.length===0 && l.hidden.length===0)"))
    throw new Error('Group deletion/show channel did not persist');
  // Cancel discards staged ordering, and layout edits preserve console values.
  await clickText('Edit channel layout');
  await expectBrowser("document.querySelector('.layout-channel')?.dataset.layoutChannel==='1'", 'Editor order missing');
  await evaluate("document.querySelector('.layout-channel button:nth-of-type(2)').click()");
  await clickText('Cancel layout changes');
  await expectBrowser("!document.querySelector('.layout-editor') && document.querySelector('.channel-strip').dataset.channel==='1'", 'Cancel changed displayed order');
  await rcp('get MIXER:Current/InCh/ToMix/Level 0 0', 'OK get MIXER:Current/InCh/ToMix/Level 0 0 -400');
  await rcp('get MIXER:Current/InCh/ToMix/On 0 0', 'OK get MIXER:Current/InCh/ToMix/On 0 0 0');
  await rcp('ssrecall_ex MIXER:Lib/Scene 2', 'OK');
  await expectBrowser("document.title === 'Show · Monitor Mix' && document.querySelector('.scene-bar').innerText.includes('Show')", 'Scene recall did not update mix page/title');
  await evaluate("[...document.querySelectorAll('button')].find(b=>b.textContent==='Choose another mix').click()");
  await expectBrowser("document.querySelectorAll('.mix-picker button').length === 2 && document.title === 'Show · Monitor Mix'", 'Picker lost current scene');
  await stop(fake); fake = null;
  await expectBrowser("document.querySelector('.scene-bar').innerText.includes('Console disconnected — scene unknown')", 'Disconnected console left stale scene name');
  await evaluate("document.querySelector('.mix-picker button').click()");
  await expectBrowser("[...document.querySelectorAll('.channel-strip input, .channel-strip button')].every(e=>e.disabled)", 'Disconnected controls remained enabled');
  ws.close();
  console.log('Browser smoke passed at 390x844 and 844x390, including performer picker, live faders/mute, saved layouts/groups and scenes.');
} finally {
  engineer?.destroy();
  await Promise.all([stop(server), stop(browser), stop(fake)]);
  await rm(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
