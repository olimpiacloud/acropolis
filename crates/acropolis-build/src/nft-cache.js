'use strict';
const Module = require('module');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const CACHE_FILE = process.env.ACROPOLIS_NFT_CACHE;
const NFT_RE = /[\\/]@vercel[\\/]nft[\\/]/;

let shared = null;

function digest(file) {
  try {
    return crypto.createHash('sha1').update(fs.readFileSync(file)).digest('hex');
  } catch {
    return null;
  }
}

function load() {
  if (shared) return shared;
  shared = new Map();
  let data;
  try {
    data = JSON.parse(fs.readFileSync(CACHE_FILE, 'utf8'));
  } catch {
    return shared;
  }
  if (!data || data.v !== 1 || !Array.isArray(data.entries)) return shared;
  for (const [file, size, hash, a] of data.entries) {
    let st;
    try {
      st = fs.statSync(file);
    } catch {
      continue;
    }
    if (st.size !== size || digest(file) !== hash) continue;
    shared.set(file, { deps: new Set(a.deps), imports: new Set(a.imports), assets: new Set(a.assets), isESM: a.isESM });
  }
  return shared;
}

function save() {
  if (!shared) return;
  const entries = [];
  for (const [file, a] of shared) {
    if (!a || typeof a !== 'object' || !a.deps) continue;
    let st;
    try {
      st = fs.statSync(file);
    } catch {
      continue;
    }
    const hash = digest(file);
    if (!hash) continue;
    entries.push([file, st.size, hash, { deps: [...a.deps], imports: [...(a.imports || [])], assets: [...(a.assets || [])], isESM: !!a.isESM }]);
  }
  try {
    fs.mkdirSync(path.dirname(CACHE_FILE), { recursive: true });
    const tmp = `${CACHE_FILE}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify({ v: 1, entries }));
    fs.renameSync(tmp, CACHE_FILE);
  } catch {}
}

function lstatReadlink(readlink) {
  return async (p) => {
    let st;
    try {
      st = fs.lstatSync(p, { throwIfNoEntry: false });
    } catch {
      return null;
    }
    if (!st || !st.isSymbolicLink()) return null;
    return readlink(p);
  };
}

function tracingRoot(dir) {
  const locks = ['pnpm-lock.yaml', 'package-lock.json', 'yarn.lock', 'bun.lock', 'bun.lockb'];
  let cur = dir;
  for (;;) {
    if (locks.some((l) => fs.existsSync(path.join(cur, l)))) return cur;
    const up = path.dirname(cur);
    if (up === cur) return dir;
    cur = up;
  }
}

const PREWARM_IGNORES = [
  '/next/dist/compiled/webpack/',
  '/node_modules/webpack5/',
  '/next/dist/server/lib/route-resolver',
  '/next/dist/pages/',
  '/next/dist/compiled/@ampproject/',
];

async function runPrewarm(cwd) {
  const req = Module.createRequire(path.join(cwd, 'package.json'));
  const nft = req('next/dist/compiled/@vercel/nft');
  const entries = [
    'next/dist/server/next-server',
    'next/dist/server/next',
    'next/dist/server/lib/start-server',
    'next/dist/server/require-hook',
    'next/dist/compiled/next-server/server.runtime.prod',
  ]
    .map((e) => {
      try {
        return req.resolve(e);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
  if (!entries.length) return;
  const analysisCache = load();
  const base = tracingRoot(cwd);
  await nft.nodeFileTrace(entries, {
    base,
    processCwd: cwd,
    mixedModules: true,
    cache: { analysisCache },
    readlink: lstatReadlink((p) => fs.promises.readlink(p).catch(() => null)),
    ignore: (p) => {
      const abs = path.isAbsolute(p) ? p : path.join(base, p);
      if (!abs.startsWith(base)) return true;
      if (/\.(d\.ts|map)$/.test(abs) || /\.development\.js$/.test(abs) || /next-server[\\/].*\.dev\.js$/.test(abs)) return true;
      return PREWARM_IGNORES.some((i) => abs.includes(i));
    },
  });
}

let prewarm = null;
let prewarmWorker = null;

function startPrewarm() {
  const { Worker, isMainThread } = require('worker_threads');
  if (!isMainThread) return;
  const bin = process.argv[1] || '';
  if (!/[\\/]next[\\/]dist[\\/]bin[\\/]next$/.test(bin) && !/[\\/]\.bin[\\/]next$/.test(bin)) return;
  if (process.argv[2] !== 'build') return;
  const worker = new Worker(
    `const m = require(${JSON.stringify(__filename)}); m.prewarmInWorker(${JSON.stringify(process.cwd())}).then(() => process.exit(0), () => process.exit(0));`,
    { eval: true, env: Object.assign({}, process.env, { ACROPOLIS_NFT_PREWARM_WORKER: '1' }) },
  );
  worker.unref();
  prewarmWorker = worker;
  prewarm = new Promise((resolve) => {
    worker.once('exit', resolve);
    worker.once('error', resolve);
  });
}

module.exports.prewarmInWorker = async (cwd) => {
  try {
    await runPrewarm(cwd);
  } finally {
    save();
  }
};

function wrap(original) {
  return async function nodeFileTrace(files, opts) {
    const options = Object.assign({}, opts);
    const cache = Object.assign({}, options.cache);
    if (prewarm) {
      if (prewarmWorker) prewarmWorker.ref();
      await prewarm;
      prewarm = null;
    }
    const analysis = load();
    if (cache.analysisCache && cache.analysisCache !== analysis) {
      for (const [k, v] of cache.analysisCache) analysis.set(k, v);
    }
    cache.analysisCache = analysis;
    options.cache = cache;
    options.readlink = lstatReadlink(options.readlink || ((p) => fs.promises.readlink(p).catch(() => null)));
    const result = await original.call(this, files, options);
    save();
    return result;
  };
}

if (CACHE_FILE && !process.env.ACROPOLIS_NFT_PREWARM_WORKER) {
  startPrewarm();
}

if (CACHE_FILE && !process.env.ACROPOLIS_NFT_PREWARM_WORKER) {
  const proxies = new WeakMap();
  const originalLoad = Module._load;
  Module._load = function (request, parent, isMain) {
    const exp = originalLoad.apply(this, arguments);
    if (!exp || typeof exp.nodeFileTrace !== 'function' || typeof request !== 'string') return exp;
    let resolved = request;
    try {
      resolved = Module._resolveFilename(request, parent, isMain);
    } catch {}
    if (!NFT_RE.test(resolved)) return exp;
    let proxy = proxies.get(exp);
    if (!proxy) {
      const wrapped = wrap(exp.nodeFileTrace);
      proxy = new Proxy(exp, { get: (t, k) => (k === 'nodeFileTrace' ? wrapped : t[k]) });
      proxies.set(exp, proxy);
    }
    return proxy;
  };
}
