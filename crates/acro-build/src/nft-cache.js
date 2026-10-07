'use strict';
const Module = require('module');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const CACHE_FILE = process.env.ACRO_NFT_CACHE;
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

function wrap(original) {
  return async function nodeFileTrace(files, opts) {
    const options = Object.assign({}, opts);
    const cache = Object.assign({}, options.cache);
    const analysis = load();
    if (cache.analysisCache && cache.analysisCache !== analysis) {
      for (const [k, v] of cache.analysisCache) analysis.set(k, v);
    }
    cache.analysisCache = analysis;
    options.cache = cache;
    const readlink = options.readlink || ((p) => fs.promises.readlink(p).catch(() => null));
    options.readlink = async (p) => {
      let st;
      try {
        st = fs.lstatSync(p, { throwIfNoEntry: false });
      } catch {
        return null;
      }
      if (!st || !st.isSymbolicLink()) return null;
      return readlink(p);
    };
    const result = await original.call(this, files, options);
    save();
    return result;
  };
}

if (CACHE_FILE) {
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
