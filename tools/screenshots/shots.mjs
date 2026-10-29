// Takes the README screenshots: demo database, simulated ergometer and strap, WebKit.
// Usage: npm run shots [-- --at=<seconds>]  (--at: ride time of the title image, default 720)

import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { webkit } from 'playwright';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../..');
const images = join(root, 'docs', 'images');
const bin = join(root, 'target', 'release', process.platform === 'win32' ? 'kettler.exe' : 'kettler');
// Relative on purpose: the service page shows this path.
const dbPath = 'demo/kettler.db';
// Half of the 24 minutes of „Pyramide“, in the middle of the 170 W stage.
const titleAt = Number(process.argv.find((a) => a.startsWith('--at='))?.slice(5) ?? 720);

const IPAD = { viewport: { width: 1180, height: 820 }, deviceScaleFactor: 1, hasTouch: true };
const IPHONE = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, hasTouch: true, isMobile: true };

let server = null;
let browser = null;

async function cleanup() {
  await browser?.close().catch(() => {});
  browser = null;
  if (server && server.exitCode == null) {
    const exited = new Promise((done) => server.once('exit', done));
    server.kill();
    await exited;
  }
  server = null;
  rmSync(join(here, 'demo'), { recursive: true, force: true });
}

process.on('SIGINT', async () => {
  console.log('\nInterrupted, cleaning up');
  await cleanup();
  process.exit(130);
});

function run(cmd, args, cwd) {
  const result = spawnSync(cmd, args, { cwd, stdio: 'inherit' });
  if (result.status !== 0) throw new Error(`${cmd} ${args.join(' ')} failed`);
}

function freePort() {
  return new Promise((done, fail) => {
    const probe = createServer();
    probe.once('error', fail);
    probe.listen(0, '127.0.0.1', () => {
      const { port } = probe.address();
      probe.close(() => done(port));
    });
  });
}

async function waitForServer(base) {
  for (let i = 0; i < 100; i++) {
    if (server.exitCode != null) throw new Error('server exited at start');
    try {
      if ((await fetch(`${base}/api/version`)).ok) return;
    } catch {
      // not listening yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error('server did not start');
}

async function api(base, path) {
  const res = await fetch(`${base}${path}`);
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json();
}

async function shot(page, name) {
  // No crosshair or hover from the last click; let charts and transitions settle.
  await page.mouse.move(0, 0);
  await page.waitForTimeout(800);
  await page.screenshot({ path: join(images, name) });
  console.log(`  ${name}`);
}

/** Opens `hash` in an app page that may have been sent to the running ride first. */
async function show(page, hash) {
  await page.evaluate((h) => { location.hash = h; }, hash);
  await page.waitForFunction((h) => location.hash === h, hash);
}

/** Ride time shown in the chart head, in seconds. */
async function rideSecs(page) {
  const text = await page.locator('.chart-time').first().textContent();
  const [mm, ss] = text.split('/')[0].trim().split(':').map(Number);
  return mm * 60 + ss;
}

async function main() {
  console.log('Building the server');
  run('cargo', ['build', '--release'], root);
  rmSync(join(here, 'demo'), { recursive: true, force: true });
  mkdirSync(join(here, 'demo'));
  run(bin, ['demo-data', '--db', dbPath], here);

  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  server = spawn(bin, ['server', '--listen', `127.0.0.1:${port}`], {
    cwd: here,
    env: { ...process.env, KETTLER_DEVICE: 'sim://', KETTLER_BLUETOOTH: 'sim', KETTLER_DB: dbPath },
    stdio: 'ignore',
  });
  await waitForServer(base);

  const bob = (await api(base, '/api/persons')).find((p) => p.name === 'Bob');
  const pyramid = (await api(base, '/api/programs')).find((p) => p.name === 'Pyramide');
  const pyramids = (await api(base, `/api/rides?person_id=${bob.id}`)).filter((r) => r.program_name === 'Pyramide');
  const [newest, oldest] = [pyramids[0], pyramids[pyramids.length - 1]];

  browser = await webkit.launch();
  const ipad = await browser.newContext(IPAD);
  const page = await ipad.newPage();

  console.log('Training selection');
  await page.goto(`${base}/#/riders`);
  await page.getByRole('button', { name: 'Bob', exact: true }).click();
  await page.getByRole('button', { name: /^Pyramide/ }).waitFor();
  await shot(page, 'training-select.png');
  console.log('Programs and history');
  await show(page, `#/programs/${pyramid.id}`);
  await page.getByRole('heading', { name: 'Block 1' }).waitFor();
  await shot(page, 'programs-editor.png');
  await show(page, '#/history');
  await page.getByRole('button', { name: 'Vergleichen' }).waitFor();
  await shot(page, 'history.png');
  await show(page, `#/history/${newest.id}`);
  await page.getByRole('button', { name: 'Fahrt löschen' }).waitFor();
  // The chart with elevation strip and recovery sits below the figures.
  await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
  await shot(page, 'ride-detail.png');
  await show(page, `#/history/compare/${newest.id}/${oldest.id}`);
  await page.locator('.compare-table').waitFor();
  await shot(page, 'compare.png');
  await show(page, '#/riders');
  await page.getByRole('button', { name: /^Pyramide/ }).click();
  await page.getByText('Training starten').waitFor();
  await page.getByText(/gefahren/).waitFor();
  await shot(page, 'start-dialog.png');
  await page.getByText('Training starten').click();
  await page.waitForURL(/#\/ride$/);

  console.log(`Waiting for ride time ${Math.floor(titleAt / 60)}:${String(titleAt % 60).padStart(2, '0')}`);
  const deadline = Date.now() + (titleAt + 600) * 1000;
  while ((await rideSecs(page)) < titleAt) {
    if (Date.now() > deadline) throw new Error('ride did not reach the title time');
    await page.waitForTimeout(1000);
  }
  const phone = await (await browser.newContext(IPHONE)).newPage();
  await phone.goto(`${base}/`);
  await phone.waitForURL(/#\/ride$/);
  await phone.locator('.chart-time').first().waitFor();
  await Promise.all([shot(page, 'training-ipad.png'), shot(phone, 'training-iphone.png')]);

  console.log('Service, with the ride still running');
  const other = await ipad.newPage();
  await other.goto(`${base}/`);
  await other.waitForURL(/#\/ride$/);
  await show(other, '#/service');
  await other.getByRole('tab', { name: 'Zustand' }).click();
  await other.getByText(dbPath).waitFor();
  await shot(other, 'service-status.png');
  await other.getByRole('tab', { name: 'Brustgurt' }).click();
  await other.getByText('HRM-Dual').first().waitFor();
  await other.getByText(/verbunden/i).first().waitFor();
  await shot(other, 'brustgurt.png');
  await other.close();
}

try {
  await main();
  console.log('Done');
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await cleanup();
}
