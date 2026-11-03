#!/usr/bin/env node
// Runs against a real Chromium instance so phone layout regressions are caught
// without adding a JavaScript package dependency to this Rust workspace.
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';

const root = new URL('..', import.meta.url).pathname;
const temp = await mkdtemp(join(tmpdir(), 'cl5-browser-'));
const port = 38123;
const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const webDist = join(temp, 'web');
let server;
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
    env: { ...process.env, CL5_LISTEN: `127.0.0.1:${port}`, CL5_DATABASE_PATH: join(temp, 'test.sqlite'), CL5_WEB_DIST: webDist },
    stdio: 'ignore',
  });
  browser = spawn(chrome, ['--headless=new', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=9222', `--user-data-dir=${join(temp, 'chrome')}`], { stdio: 'ignore' });
  await ready(`http://127.0.0.1:${port}/setup`);
  await ready('http://127.0.0.1:9222/json/version');
  const target = await fetch(`http://127.0.0.1:9222/json/new?http://127.0.0.1:${port}/setup`, { method: 'PUT' }).then(r => r.json());
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
  ws.close();
  console.log('Browser smoke passed at 390x844 and 844x390.');
} finally {
  await Promise.all([stop(server), stop(browser)]);
  await rm(temp, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}
