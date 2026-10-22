// Records docs/demo.gif and the two screenshots from the sandboxed demo instance (see sandbox.sh).
// Usage: NODE_PATH=<dir with puppeteer-core>/node_modules node docs/demo/capture.cjs [all|gif|stills]
// Needs google-chrome-stable and ffmpeg. Frames go to $CAPTURE_DIR (default /tmp/everdir-capture).
const puppeteer = require('puppeteer-core');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const APP = process.env.EVERDIR_DEMO_URL || 'http://127.0.0.1:7272/';
const DOCS = path.resolve(__dirname, '..');
const FRAME = 'file://' + path.join(__dirname, 'frame.html');
const TMP = process.env.CAPTURE_DIR || path.join(os.tmpdir(), 'everdir-capture');
const what = process.argv[2] || 'all';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Deterministic jitter for the typing cadence.
let seed = 7;
const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648);

async function open(browser, w, h, pad, dpr) {
  const page = await browser.newPage();
  await page.emulateTimezone('Europe/Berlin');
  await page.setViewport({ width: w, height: h, deviceScaleFactor: dpr });
  let inflight = 0;
  page.on('request', () => inflight++);
  page.on('requestfinished', () => inflight--);
  page.on('requestfailed', () => inflight--);
  page.idle = async () => {
    await sleep(40); // the search box debounces input by 25 ms
    for (let i = 0; i < 200 && inflight > 0; i++) await sleep(10);
    await app().evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
  };
  await page.goto(`${FRAME}?pad=${pad}&src=${encodeURIComponent(APP)}`);
  const app = () => page.frames().find((f) => f.url().startsWith(APP));
  for (let i = 0; i < 100 && !app(); i++) await sleep(50);
  await app().waitForFunction(() => typeof tm !== 'undefined' && tm.R && N.size > 1 && EX.length > 0, { timeout: 15000 });
  await page.idle();
  page.app = app;
  // Offset of the iframe inside the page, to turn in-app coordinates into page coordinates.
  page.off = await page.evaluate(() => { const b = document.getElementById('app').getBoundingClientRect(); return { x: b.left, y: b.top }; });
  return page;
}

// Page coordinates of an element inside the app.
async function elAt(page, sel, fx = 0.5, fy = 0.5) {
  const b = await page.app().evaluate((s) => { const r = document.querySelector(s).getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; }, sel);
  return { x: page.off.x + b.x + b.w * fx, y: page.off.y + b.y + b.h * fy };
}
// Page coordinates of a point inside the treemap rect of the node at `p`.
async function blockAt(page, p, fx = 0.5, fy = 0.5) {
  const b = await page.app().evaluate(async (p, fx, fy) => {
    const n = await api(`/api/node?path=${encodeURIComponent(p)}&lite=1`);
    const i = tm.byId.get(n.id);
    if (i === undefined) throw new Error('no treemap block for ' + p);
    const R = tm.R, o = i * 8, c = ov.getBoundingClientRect();
    const sx = c.width / tm.W, sy = c.height / tm.H;
    return { x: c.left + (R[o] + (R[o + 2] - R[o]) * fx) * sx, y: c.top + (R[o + 1] + (R[o + 3] - R[o + 1]) * fy) * sy };
  }, p, fx, fy);
  return { x: page.off.x + b.x, y: page.off.y + b.y };
}

// ---------------------------------------------------------------- GIF
async function gif(browser) {
  const W = 1040, H = 694, FPS = 18;
  const page = await open(browser, W, H, 0, 2);
  const dir = path.join(TMP, 'frames');
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  const frames = []; // [file, seconds]
  let cur = { x: W * 0.46, y: H * 0.55 };
  const shot = async (sec) => {
    await page.evaluate((x, y) => cursorAt(x, y), cur.x, cur.y);
    const f = path.join(dir, `f${String(frames.length).padStart(4, '0')}.png`);
    await page.screenshot({ path: f });
    frames.push([f, sec]);
  };
  const hold = async (sec) => { await page.idle(); await shot(sec); };
  // Eased cursor glide, one frame per step; hovering triggers the app's own mousemove handlers.
  const glide = async (to, sec) => {
    const from = { ...cur }, n = Math.max(2, Math.round(sec * FPS));
    for (let i = 1; i <= n; i++) {
      const t = i / n, e = t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
      cur = { x: from.x + (to.x - from.x) * e, y: from.y + (to.y - from.y) * e };
      await page.mouse.move(cur.x, cur.y);
      await sleep(8);
      await shot(1 / FPS);
    }
    await page.idle();
  };
  const app = page.app();
  const dbl = async () => {
    const before = await app.evaluate(() => tm.path);
    // The first double-click after the search view sometimes only registers as a click; retry
    // (no frames are taken in between, so a retry is invisible in the GIF).
    for (let i = 0; i < 3; i++) {
      await page.mouse.click(cur.x, cur.y, { count: 2, delay: 30 });
      if (await app.waitForFunction((b) => tm.path !== b, { timeout: 1500 }, before).then(() => true, () => false)) break;
      await sleep(400);
    }
    console.log('zoomed to', await app.evaluate(() => tm.path));
  };

  await hold(0.5);
  await glide(await elAt(page, '#q', 0.3), 0.55);
  await page.mouse.click(cur.x, cur.y);
  await hold(0.15);
  for (const ch of 'lisbon') {
    await page.keyboard.type(ch);
    await hold(0.07 + rnd() * 0.1);
  }
  await hold(0.8);
  await glide(await elAt(page, '#mhead span[data-s="size"]', 0.6), 0.45);
  await page.mouse.click(cur.x, cur.y);
  await hold(1.0);
  await glide(await elAt(page, '#q', 0.2), 0.45);
  await page.mouse.click(cur.x, cur.y);
  await page.keyboard.press('Escape');
  await hold(0.35);
  await glide(await blockAt(page, '/home/alex/.local/share/Steam', 0.45, 0.55), 0.6);
  await hold(0.3);
  await glide(await blockAt(page, '/home/alex/Videos', 0.55, 0.4), 0.45);
  await dbl();
  await hold(0.55);
  await dbl();
  await hold(0.55);
  await glide(await blockAt(page, '/home/alex/Videos', 0.5, 0.5), 0.35);
  await dbl();
  await hold(0.6);
  const movie = await app.evaluate(async () => {
    const r = await api('/api/search?q=' + encodeURIComponent('2160p.mkv') + '&sort=size&desc=1&limit=1');
    return r.results[0].dir + '/' + r.results[0].name;
  });
  await glide(await blockAt(page, movie, 0.5, 0.5), 0.5);
  await page.mouse.click(cur.x, cur.y);
  await hold(1.8);
  await page.close();

  // Variable-duration frames -> constant-rate GIF with a shared palette.
  const list = frames.map(([f, s]) => `file '${f}'\nduration ${s.toFixed(3)}`).join('\n') + `\nfile '${frames.at(-1)[0]}'\n`;
  fs.writeFileSync(path.join(TMP, 'frames.txt'), list);
  const out = path.join(DOCS, 'demo.gif');
  const vf = `fps=${FPS},scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=256:stats_mode=diff[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle`;
  execFileSync('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'concat', '-safe', '0', '-i', path.join(TMP, 'frames.txt'), '-vf', vf, '-loop', '0', out], { stdio: 'inherit' });
  const total = frames.reduce((a, f) => a + f[1], 0);
  console.log(`demo.gif: ${frames.length} frames, ${total.toFixed(1)} s, ${(fs.statSync(out).size / 1e6).toFixed(2)} MB`);
}

// ---------------------------------------------------------------- stills
async function stills(browser) {
  const W = 1320, H = 840, PAD = 28;
  let page = await open(browser, W, H, PAD, 2);
  let app = page.app();
  await app.evaluate(() => { const q = document.querySelector('#q'); q.value = 'ext:mp4,mkv size:>2g'; sort = 'size'; desc = true; doSearch(); });
  await page.idle();
  await app.evaluate(() => { const r = curRows(); select(r[2].id, true); });
  await page.idle();
  await page.screenshot({ path: path.join(DOCS, 'screenshot-search.png'), omitBackground: true });
  await page.close();

  page = await open(browser, W, H, PAD, 2);
  app = page.app();
  await app.evaluate(async () => {
    document.body.style.setProperty('--top', '34vh');
    await reveal((await api('/api/node?path=/home/alex/Videos&lite=1')).id);
  });
  await page.idle();
  await sleep(300); // the treemap reloads 120 ms after a resize
  await page.idle();
  const p = await blockAt(page, '/home/alex/.local/share/Steam', 0.4, 0.5);
  await page.mouse.move(p.x, p.y);
  await page.idle();
  await page.screenshot({ path: path.join(DOCS, 'screenshot-treemap.png'), omitBackground: true });
  await page.close();
  console.log('screenshots written');
}

(async () => {
  fs.mkdirSync(TMP, { recursive: true });
  const browser = await puppeteer.launch({ executablePath: process.env.CHROME || '/usr/bin/google-chrome-stable', headless: true,
    args: ['--allow-file-access-from-files', '--font-render-hinting=none', '--hide-scrollbars'] });
  try {
    if (what === 'all' || what === 'stills') await stills(browser);
    if (what === 'all' || what === 'gif') await gif(browser);
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error(e); process.exit(1); });
