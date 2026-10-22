// Writes a synthetic everdir scan cache (a made-up Linux disk, nothing read from the real one).
// Usage: node gen-data.mjs <cache-dir>   (writes <cache-dir>/everdir/drive-2f.bin; needs the `lz4` CLI)
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

// ---------------------------------------------------------------- deterministic randomness
let seed = 0x5eed1234;
const rnd = () => {
  seed |= 0; seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const ri = (a, b) => a + Math.floor(rnd() * (b - a + 1));
const pick = (a) => a[Math.floor(rnd() * a.length)];
const pad = (n, w) => String(n).padStart(w, '0');
const KB = 1024, MB = KB * 1024, GB = MB * 1024;
const NOW = Date.UTC(2026, 9, 16, 8, 40) / 1000;
const ago = (maxDays, minDays = 0) => Math.floor(NOW - (minDays + rnd() * (maxDays - minDays)) * 86400);
const logSize = (lo, hi) => Math.floor(Math.exp(Math.log(lo) + rnd() * (Math.log(hi) - Math.log(lo))));

// ---------------------------------------------------------------- tree helpers
const uniq = (list) => { const seen = new Set(); return list.filter((x) => !seen.has(x.name) && seen.add(x.name)); };
const dir = (name, children = [], mtime = ago(400)) => ({ name, dir: true, children: uniq(children.flat().filter(Boolean)), mtime });
const file = (name, size, mtime = ago(700)) => ({ name, dir: false, size: Math.floor(size), mtime });
const many = (n, f) => Array.from({ length: n }, (_, i) => f(i));

const WORDS = ['core', 'util', 'index', 'parser', 'render', 'config', 'state', 'event', 'store', 'client', 'server', 'model',
  'view', 'router', 'cache', 'stream', 'buffer', 'queue', 'worker', 'logger', 'format', 'layout', 'theme', 'icon', 'button',
  'dialog', 'table', 'chart', 'auth', 'session', 'socket', 'schema', 'token', 'reader', 'writer', 'encoder', 'decoder', 'math',
  'vector', 'matrix', 'color', 'image', 'audio', 'video', 'font', 'shader', 'mesh', 'scene', 'input', 'timer', 'pool', 'hash'];
const word = () => pick(WORDS);
const ident = () => word() + (rnd() < 0.5 ? '-' + word() : '');

// npm-ish dependency tree
const NPM = ['react', 'react-dom', 'scheduler', 'lodash', 'typescript', 'esbuild', 'vite', 'rollup', 'postcss', 'tailwindcss',
  'eslint', 'prettier', 'chalk', 'commander', 'debug', 'semver', 'glob', 'minimatch', 'picomatch', 'acorn', 'terser', 'zod',
  'axios', 'date-fns', 'uuid', 'yaml', 'ws', 'undici', 'jsdom', 'vitest', '@babel/core', '@babel/parser', '@types/node',
  '@types/react', 'caniuse-lite', 'browserslist', 'electron-to-chromium', 'core-js', 'rxjs', 'tslib', 'source-map', 'nanoid'];
function npmPkg(name) {
  const files = many(ri(6, 60), () => file(`${ident()}.${pick(['js', 'js', 'js', 'mjs', 'cjs', 'd.ts', 'map', 'json'])}`, logSize(300, 180 * KB)));
  return dir(name.split('/').pop(), [file('package.json', ri(600, 4000)), file('README.md', ri(1000, 20000)), file('LICENSE', 1070),
    dir('dist', uniq(files)), rnd() < 0.4 && dir('lib', uniq(many(ri(4, 30), () => file(`${ident()}.js`, logSize(400, 60 * KB)))))]);
}
function nodeModules(scale = 1) {
  const pkgs = NPM.filter(() => rnd() < 0.85);
  for (let i = 0; i < 160 * scale; i++) pkgs.push(`${ident()}-${pick(['js', 'lite', 'utils', 'plugin', 'loader', 'core'])}`);
  const scoped = {};
  const top = [];
  for (const p of new Set(pkgs)) {
    if (p.startsWith('@')) { const [s, n] = p.split('/'); (scoped[s] ||= []).push(npmPkg(n)); } else top.push(npmPkg(p));
  }
  for (const [s, k] of Object.entries(scoped)) top.push(dir(s, k));
  if (pkgs.includes('esbuild')) top.push(dir('@esbuild', [dir('linux-x64', [dir('bin', [file('esbuild', 9.8 * MB)])])]));
  return dir('node_modules', top, ago(30));
}

// Rust project with a target dir
function rustTarget(name) {
  const deps = many(ri(80, 260), () => `${ident().replace(/-/g, '_')}-${(rnd() * 2 ** 32 >>> 0).toString(16).padStart(8, '0')}${(rnd() * 2 ** 32 >>> 0).toString(16).padStart(8, '0')}`);
  const prof = (p, big) => dir(p, [
    dir('deps', uniq(deps.flatMap((d) => [file(`lib${d}.rlib`, logSize(80 * KB, big ? 90 * MB : 30 * MB)), file(`lib${d}.rmeta`, logSize(20 * KB, 8 * MB)), file(`${d}.d`, ri(300, 9000))]))),
    dir('build', many(ri(10, 30), (i) => dir(`${ident()}-${pad(i, 4)}`, [file('output', ri(0, 4000)), file('stderr', 0), file('root-output', 60)]))),
    dir('incremental', many(ri(4, 12), (i) => dir(`${name.replace(/-/g, '_')}-${pad(i, 3)}`, many(ri(20, 90), (j) => file(`s-${pad(j, 5)}.o`, logSize(10 * KB, 4 * MB)))))),
    file(name, logSize(4 * MB, 40 * MB), ago(5)),
  ], ago(5));
  return dir('target', [prof('debug', true), rnd() < 0.7 && prof('release', false), file('.rustc_info.json', 1400), file('CACHEDIR.TAG', 177)], ago(5));
}
function project(name, kind) {
  const src = uniq(many(ri(12, 70), () => file(`${ident().replace(/-/g, '_')}.${kind === 'rust' ? 'rs' : kind === 'py' ? 'py' : pick(['ts', 'tsx', 'css'])}`, logSize(400, 40 * KB), ago(120))));
  const k = [file('README.md', ri(800, 9000), ago(90)), file('LICENSE', 1070), file('.gitignore', ri(20, 300)),
    dir('.git', [dir('objects', many(ri(30, 120), (i) => dir(pad(i.toString(16), 2), uniq(many(ri(2, 30), () => file((rnd() * 2 ** 52).toString(16).slice(0, 38), logSize(80, 200 * KB))))))),
      file('HEAD', 23), file('config', 310), file('index', logSize(4 * KB, 900 * KB)), dir('refs', [dir('heads', [file('main', 41)])])]), dir('src', src)];
  if (kind === 'rust') k.push(file('Cargo.toml', ri(300, 1500)), file('Cargo.lock', ri(9000, 90000)), rustTarget(name));
  if (kind === 'web') k.push(file('package.json', ri(600, 2500)), file('package-lock.json', ri(80 * KB, 600 * KB)), file('tsconfig.json', 700), nodeModules(2), dir('public', many(ri(4, 20), () => file(`${word()}.${pick(['png', 'svg', 'webp'])}`, logSize(2 * KB, 900 * KB)))));
  if (kind === 'py') {
    const sitePkgs = many(ri(40, 90), () => dir(ident().replace(/-/g, '_'), many(ri(4, 60), () => file(`${word()}.${pick(['py', 'py', 'pyc', 'so'])}`, logSize(500, 2 * MB)))));
    k.push(file('pyproject.toml', ri(400, 1600)), dir('.venv', [dir('lib', [dir('python3.13', [dir('site-packages', sitePkgs)])])]),
      dir('data', many(ri(3, 12), (i) => file(`dataset-${pad(i, 2)}.${pick(['csv', 'parquet'])}`, logSize(2 * MB, 3 * GB)))));
  }
  return dir(name, k, ago(20));
}

// ---------------------------------------------------------------- home
const HOME = 'alex';
const cities = [['lisbon', 2024], ['iceland', 2023], ['kyoto', 2025], ['dolomites', 2022], ['berlin', 2025]];
const pictures = dir('Pictures', [
  ...cities.map(([c, y]) => dir(`${y} ${c[0].toUpperCase() + c.slice(1)}`, many(ri(300, 900), (i) => file(`${c}_${y}_${pad(i + 1, 4)}.${rnd() < 0.15 ? 'cr3' : 'jpg'}`, 0, ago(1200, 30))).map((f) => (f.size = f.name.endsWith('cr3') ? logSize(22 * MB, 34 * MB) : logSize(3 * MB, 9 * MB), f)), ago(900, 30))),
  dir('Screenshots', many(1240, (i) => { const t = new Date((NOW - i * 41000 - ri(0, 30000)) * 1000); const s = t.toISOString().replace(/[-:]/g, '').replace('T', '_').slice(0, 15); return file(`Screenshot_${s}.png`, logSize(90 * KB, 4 * MB), Math.floor(t / 1000)); })),
  dir('Wallpapers', many(64, (i) => file(`wallpaper-${pad(i + 1, 3)}.${pick(['jpg', 'png'])}`, logSize(1 * MB, 14 * MB)))),
  dir('Phone', many(2200, (i) => file(`IMG_${20230101 + Math.floor(i / 6)}_${pad(ri(0, 235959), 6)}.jpg`, logSize(2 * MB, 7 * MB), ago(900)))),
]);
const videos = dir('Videos', [
  ...cities.map(([c, y]) => {
    const C = c[0].toUpperCase() + c.slice(1);
    return dir(`${C} ${y}`, [...many(ri(5, 10), (i) => file(`${C} ${y} - Day ${i + 1}.mp4`, logSize(1.2 * GB, 6 * GB), ago(900, 60))),
      ...many(ri(12, 30), (i) => file(`GX01${pad(ri(1000, 9999), 4)}.MP4`, logSize(300 * MB, 2.5 * GB), ago(900, 60))), file(`${C} ${y} (4K edit).mkv`, logSize(12 * GB, 28 * GB), ago(800, 30))]);
  }),
  dir('Recordings', many(30, (i) => file(`recording-2026-${pad(ri(1, 9), 2)}-${pad(ri(1, 28), 2)}_${pad(i, 2)}.mkv`, logSize(300 * MB, 6 * GB), ago(280)))),
  dir('Movies', many(14, (i) => file(`${pick(['The', 'A', 'Last', 'Night', 'Blue', 'Silent', 'Long', 'Iron'])} ${pick(['Harbor', 'Signal', 'Orchard', 'Frontier', 'Voyage', 'Meridian', 'Lantern', 'Tide'])} (${ri(1984, 2025)}) ${pick(['1080p', '2160p'])}.mkv`, logSize(4 * GB, 40 * GB)))),
  dir('Family', many(160, (i) => file(`family-${pad(i + 1, 3)}.mp4`, logSize(60 * MB, 1.2 * GB)))),
]);
const music = dir('Music', many(48, () => {
  const a = `${pick(['North', 'Glass', 'Paper', 'Velvet', 'Echo', 'Low', 'Golden', 'Static'])} ${pick(['Lights', 'Rivers', 'Engines', 'Animals', 'Parade', 'Season', 'Choir', 'Coast'])}`;
  return dir(a, many(ri(1, 5), () => dir(`${ri(1995, 2025)} - ${pick(['Arrivals', 'Night Drive', 'Afterglow', 'Field Notes', 'Undertow', 'Satellites', 'Blue Hour', 'Halflight'])}`,
    [...many(ri(8, 16), (i) => file(`${pad(i + 1, 2)} - ${pick(['Intro', 'Runaway', 'Signals', 'Shoreline', 'Fever', 'Weightless', 'Homecoming', 'Paper Moon', 'Embers', 'Outro'])}.flac`, logSize(18 * MB, 60 * MB))), file('cover.jpg', logSize(300 * KB, 3 * MB))])));
}));
const docs = dir('Documents', [
  dir('Invoices', many(84, (i) => file(`invoice-${2019 + Math.floor(i / 12)}-${pad((i % 12) + 1, 2)}.pdf`, logSize(40 * KB, 400 * KB)))),
  dir('Taxes', many(7, (i) => file(`tax-return-${2019 + i}.pdf`, logSize(200 * KB, 3 * MB)))),
  dir('Manuals', many(40, () => file(`${word()}-manual.pdf`, logSize(1 * MB, 40 * MB)))),
  file('lisbon-boarding-pass.pdf', 182 * KB, ago(800)), file('lisbon-itinerary.odt', 46 * KB, ago(810)),
  file('cv-2026.pdf', 210 * KB, ago(40)), file('budget-2026.ods', 88 * KB, ago(3)), file('notes.md', 14 * KB, ago(1)),
]);
const downloads = dir('Downloads', [
  file('archlinux-2026.10.01-x86_64.iso', 1.3 * GB, ago(15)), file('ubuntu-24.04.3-desktop-amd64.iso', 6.1 * GB, ago(200)),
  file('Fedora-Workstation-Live-43.iso', 2.4 * GB, ago(120)), file('win11_24H2_x64.iso', 5.8 * GB, ago(300)),
  ...many(60, () => file(`${ident()}-${ri(1, 9)}.${ri(0, 20)}.${pick(['tar.gz', 'zip', 'AppImage', 'deb', 'tar.xz'])}`, logSize(200 * KB, 900 * MB))),
  ...many(40, () => file(`${word()}_${ri(100, 999)}.${pick(['pdf', 'png', 'jpg', 'csv', 'zip'])}`, logSize(20 * KB, 40 * MB))),
  file('lisbon-tram-map.png', 2.1 * MB, ago(820)),
]);
const GAMES = [['Starfall Odyssey', 128], ['Iron Harbor', 74], ['Neon Drift', 46], ['Hollow Kingdoms', 92], ['Skyforge Tactics', 23],
  ['Pale Meridian', 61], ['Tidebreakers', 18], ['Ashen Frontier', 109], ['Little Lanterns', 4], ['Orbital Freight Co', 9]];
const steam = dir('Steam', [dir('steamapps', [
  dir('common', GAMES.map(([g, gb]) => {
    const n = ri(8, 30), each = (gb * GB * 0.9) / n;
    return dir(g, [dir('Content', [dir('Paks', many(n, (i) => file(`${g.replace(/\W/g, '')}-${pick(['Windows', 'Linux'])}-${pad(i, 3)}.pak`, each * (0.4 + rnd() * 1.2))))]),
      dir('Binaries', many(ri(10, 60), () => file(`${ident()}.${pick(['dll', 'so', 'exe', 'bin'])}`, logSize(100 * KB, 120 * MB)))),
      dir('Movies', many(ri(4, 20), (i) => file(`cinematic_${pad(i, 2)}.bk2`, logSize(40 * MB, 900 * MB)))),
      ...many(ri(50, 400), () => file(`${word()}_${pad(ri(0, 999), 3)}.${pick(['vpk', 'bin', 'ogg', 'json', 'dat', 'txt'])}`, logSize(1 * KB, 300 * MB)))]);
  })),
  dir('shadercache', many(10, (i) => dir(String(1000000 + i * 7919), [file('fozpipelinesv6.foz', logSize(30 * MB, 1.5 * GB))]))),
  dir('compatdata', many(10, (i) => dir(String(1000000 + i * 7919), [dir('pfx', many(ri(200, 900), () => file(`${word()}.${pick(['dll', 'exe', 'reg', 'nls'])}`, logSize(10 * KB, 30 * MB))))]))),
  dir('downloading', []),
])]);
const dotcache = dir('.cache', [
  dir('mozilla', [dir('firefox', [dir('x7a1b2c3.default-release', [dir('cache2', [dir('entries', many(12000, () => file((rnd() * 2 ** 52).toString(16).toUpperCase().slice(0, 40), logSize(200, 2 * MB))))])])])]),
  dir('pip', [dir('http-v2', many(800, () => file((rnd() * 2 ** 52).toString(16).slice(0, 32) + '.body', logSize(5 * KB, 80 * MB))))]),
  dir('thumbnails', [dir('large', many(3600, () => file((rnd() * 2 ** 52).toString(16).slice(0, 32) + '.png', logSize(8 * KB, 90 * KB))))]),
  dir('yarn', many(1400, () => file(`npm-${ident()}-${ri(1, 9)}.${ri(0, 20)}.${ri(0, 9)}.tgz`, logSize(2 * KB, 6 * MB)))),
]);
const dotlocal = dir('.local', [dir('share', [steam,
  dir('flatpak', [dir('app', many(14, () => dir(`org.${word()}.${word()[0].toUpperCase() + word().slice(1)}`, many(ri(40, 300), () => file(`${word()}.${pick(['so', 'png', 'svg', 'mo', 'json'])}`, logSize(1 * KB, 60 * MB))))))]),
  dir('Trash', [dir('files', many(30, () => file(`${word()}-${ri(1, 99)}.${pick(['mkv', 'zip', 'jpg', 'iso'])}`, logSize(1 * MB, 4 * GB))))]),
])]);
const home = dir(HOME, [
  dir('Projects', [project('rust-raytracer', 'rust'), project('home-dashboard', 'web'), project('photo-sorter', 'py'), project('budget-app', 'web'),
    project('tiny-shell', 'rust'), project('blog', 'web'), project('weather-station', 'py'), project('ledger-cli', 'rust')]),
  videos, pictures, music, docs, downloads, dotcache, dotlocal,
  dir('VMs', [file('win11-dev.qcow2', 64 * GB, ago(20)), file('ubuntu-server.qcow2', 22 * GB, ago(60)), file('fedora-test.qcow2', 14 * GB, ago(140))]),
  dir('.cargo', [dir('registry', [dir('src', many(420, () => dir(`${ident()}-${ri(0, 4)}.${ri(0, 30)}.${ri(0, 9)}`, many(ri(4, 40), () => file(`${word()}.rs`, logSize(500, 120 * KB)))))), dir('cache', many(420, () => file(`${ident()}-${ri(0, 4)}.${ri(0, 30)}.${ri(0, 9)}.crate`, logSize(5 * KB, 3 * MB))))])]),
  dir('.mozilla', [dir('firefox', [dir('x7a1b2c3.default-release', many(60, () => file(`${word()}.${pick(['sqlite', 'json', 'db'])}`, logSize(4 * KB, 200 * MB))))])]),
  file('.bashrc', 3700), file('.zsh_history', 420 * KB, ago(0.1)),
]);

// ---------------------------------------------------------------- system
const usrLib = dir('lib', [
  ...many(4800, () => file(`lib${ident()}.so.${ri(0, 9)}.${ri(0, 30)}.${ri(0, 9)}`, logSize(20 * KB, 40 * MB))),
  dir('firmware', many(1800, () => file(`${word()}_${pick(['fw', 'ucode', 'bin'])}_${pad(ri(0, 999), 3)}.${pick(['bin', 'zst'])}`, logSize(4 * KB, 8 * MB)))),
  dir('python3.13', many(1400, () => dir(ident().replace(/-/g, '_'), uniq(many(ri(3, 40), () => file(`${word()}.${pick(['py', 'py', 'pyc', 'so'])}`, logSize(500, 900 * KB))))))),
  dir('modules', [dir('6.17.2-arch1-1', [dir('kernel', many(5600, () => file(`${ident()}.ko.zst`, logSize(4 * KB, 4 * MB))))])]),
  dir('llvm', many(30, () => file(`lib${ident()}.so`, logSize(10 * MB, 140 * MB)))),
  dir('nvidia', many(40, () => file(`lib${ident()}.so.580.95`, logSize(1 * MB, 280 * MB)))),
]);
const usrShare = dir('share', [
  dir('icons', many(60, () => dir(`${word()}-${pick(['icons', 'theme', 'dark'])}`, many(8, (s) => dir(`${[16, 22, 24, 32, 48, 64, 128, 256][s]}x${[16, 22, 24, 32, 48, 64, 128, 256][s]}`, uniq(many(ri(60, 260), () => file(`${ident()}.${pick(['png', 'svg'])}`, logSize(300, 60 * KB))))))))),
  dir('locale', many(120, (i) => dir(`${String.fromCharCode(97 + (i % 26))}${String.fromCharCode(97 + ((i * 7) % 26))}`, [dir('LC_MESSAGES', many(ri(40, 160), () => file(`${ident()}.mo`, logSize(2 * KB, 400 * KB))))]))),
  dir('doc', many(2000, () => dir(ident(), many(ri(1, 12), () => file(`${word()}.${pick(['html', 'txt', 'md', 'gz'])}`, logSize(500, 600 * KB)))))),
  dir('fonts', many(600, () => file(`${word()}-${pick(['Regular', 'Bold', 'Italic', 'Light'])}.${pick(['ttf', 'otf', 'woff2'])}`, logSize(30 * KB, 20 * MB)))),
  dir('man', many(9, (i) => dir(`man${i + 1}`, many(ri(200, 1600), () => file(`${ident()}.${i + 1}.gz`, logSize(500, 40 * KB)))))),
]);
const root = dir('/', [
  dir('home', [home]),
  dir('usr', [dir('bin', many(3200, () => file(ident(), logSize(8 * KB, 120 * MB)))), usrLib, usrShare,
    dir('include', many(700, () => dir(ident(), uniq(many(ri(3, 60), () => file(`${word()}.h`, logSize(300, 200 * KB))))))),
    dir('src', [dir('linux-6.17.2', many(1200, () => dir(ident(), uniq(many(ri(10, 60), () => file(`${word()}.${pick(['c', 'h', 'o'])}`, logSize(500, 400 * KB)))))))])]),
  dir('var', [
    dir('cache', [dir('pacman', [dir('pkg', many(2600, () => file(`${ident()}-${ri(0, 9)}.${ri(0, 40)}-${ri(1, 4)}-x86_64.pkg.tar.zst`, logSize(20 * KB, 90 * MB))))])]),
    dir('lib', [dir('docker', [dir('overlay2', many(220, () => dir((rnd() * 2 ** 52).toString(16).slice(0, 32), [dir('diff', many(ri(20, 300), () => file(`${word()}.${pick(['so', 'py', 'js', 'bin', 'conf'])}`, logSize(500, 12 * MB))))])))]),
      dir('libvirt', [dir('images', [file('windows-server.qcow2', 48 * GB, ago(90))])]), dir('flatpak', [dir('runtime', many(10, () => dir(`org.${word()}.Platform`, many(ri(1500, 4000), () => file(`${ident()}.${pick(['so', 'py', 'png', 'mo', 'conf'])}`, logSize(500, 30 * MB))))))])]),
    dir('log', [dir('journal', many(60, (i) => file(`system@${pad(i, 4)}.journal`, logSize(8 * MB, 128 * MB))))]),
  ]),
  dir('opt', [dir('google', [dir('chrome', many(120, () => file(`${ident()}.${pick(['so', 'pak', 'bin', 'dat'])}`, logSize(100 * KB, 240 * MB))))]), dir('cuda', many(400, () => file(`lib${ident()}.so.12`, logSize(1 * MB, 900 * MB))))]),
  dir('boot', [file('vmlinuz-linux', 15 * MB), file('initramfs-linux.img', 38 * MB), file('initramfs-linux-fallback.img', 140 * MB), file('amd-ucode.img', 84 * KB)]),
  dir('swap', [file('swapfile', 32 * GB, ago(500))]),
  dir('etc', many(900, () => file(`${ident()}.${pick(['conf', 'conf', 'list', 'xml', 'json'])}`, logSize(100, 40 * KB)))),
], NOW - 86400 * 30);

// ---------------------------------------------------------------- encode (drives.rs: encode / save_cache)
const chunks = [];
let buf = Buffer.alloc(1 << 20), pos = 0;
const flush = () => { chunks.push(buf.subarray(0, pos)); buf = Buffer.alloc(1 << 20); pos = 0; };
const byte = (b) => { if (pos >= buf.length) flush(); buf[pos++] = b; };
const varint = (v) => { v = BigInt(Math.max(0, Math.floor(v))); while (v >= 0x80n) { byte(Number(v & 0x7fn) | 0x80); v >>= 7n; } byte(Number(v)); };
let nodes = 0, files = 0, alloc = 0;
function enc(t) {
  nodes++;
  const name = Buffer.from(t.name);
  byte(t.dir ? 1 : 0); varint(name.length); for (const b of name) byte(b);
  if (t.dir) { varint(t.mtime); varint(t.children.length); for (const c of t.children) enc(c); }
  else { const a = Math.ceil(t.size / 4096) * 4096; files++; alloc += a; varint(t.size); varint(a); varint(t.mtime); }
}
enc(root);
flush();
const body = Buffer.concat(chunks);
const lz = execFileSync('lz4', ['-c', '-q'], { input: body, maxBuffer: 1 << 30 });
const head = Buffer.alloc(8 + 6 * 8);
Buffer.from('EVDIR\x00\x00\x01', 'latin1').copy(head);
const scanMs = 1900;
[NOW, scanMs, 0, nodes, alloc, files].forEach((v, i) => head.writeBigUInt64LE(BigInt(Math.floor(v)), 8 + i * 8));
const out = join(process.argv[2] || '.', 'everdir');
mkdirSync(out, { recursive: true });
writeFileSync(join(out, 'drive-2f.bin'), Buffer.concat([head, lz]));
console.log(`${files.toLocaleString()} files, ${nodes.toLocaleString()} nodes, ${(alloc / GB).toFixed(1)} GiB`);
if (process.env.EVERDIR_DEMO_STATS) {
  const sum = (t) => t.dir ? (t._s ??= t.children.reduce((a, c) => a + sum(c), 0)) : t.size;
  const cnt = (t) => t.dir ? t.children.reduce((a, c) => a + cnt(c), 0) : 1;
  const show = (t, d, p) => { if (d > +process.env.EVERDIR_DEMO_STATS || !t.dir) return; for (const c of [...t.children].sort((a, b) => sum(b) - sum(a)).slice(0, 12)) { if (sum(c) > 5 * GB) console.log(`${'  '.repeat(d)}${(sum(c) / GB).toFixed(0).padStart(5)} G ${String(cnt(c)).padStart(7)}  ${c.name}`); show(c, d + 1); } };
  show(root, 0);
}
