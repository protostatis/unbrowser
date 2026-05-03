// Auto-strategy extraction. Tries a fixed sequence of public-knowledge
// strategies, scores each by data richness, returns the best one. No
// private dependency — all strategies use only standard DOM/JSON shapes
// the open web uses to expose structured data.
//
// Strategy order is roughly "highest signal-to-noise first":
//   1. json_ld    — <script type="application/ld+json"> (Schema.org)
//   2. next_data  — Next.js __NEXT_DATA__ JSON blob
//   3. nuxt_data  — Nuxt __NUXT__ blob
//   4. og_meta    — OpenGraph + Twitter Card + standard meta tags
//   5. microdata  — itemscope/itemprop walk (Schema.org-in-HTML)
//   6. text_main  — chrome-stripped main content (always available as fallback)
//
// Each returns { strategy, confidence, data, hint? } or null. Confidence is
// a rough 0..1 — JSON-LD with Article > 0.9, OG with title+description ~0.6,
// text_main fallback ~0.3.

(function () {
  function safeJSONParse(s) {
    try { return JSON.parse(s); } catch { return null; }
  }

  function strategyJsonLd() {
    var nodes = document.querySelectorAll('script[type="application/ld+json"]');
    if (!nodes.length) return null;
    var blobs = [];
    for (var i = 0; i < nodes.length; i++) {
      var raw = nodes[i].textContent || '';
      if (!raw.trim()) continue;
      var parsed = safeJSONParse(raw);
      if (parsed) blobs.push(parsed);
    }
    if (!blobs.length) return null;
    // Single object → return directly. Array of @graph entries → flatten.
    var data = blobs.length === 1 ? blobs[0] : blobs;
    // Confidence: if any blob has @type set to a real schema (Article, Product,
    // Recipe, etc.), it's high-signal. Otherwise medium.
    var hasType = blobs.some(function (b) {
      if (b && b['@type']) return true;
      if (b && b['@graph'] && Array.isArray(b['@graph'])) {
        return b['@graph'].some(function (g) { return g && g['@type']; });
      }
      return false;
    });
    return {
      strategy: 'json_ld',
      confidence: hasType ? 0.95 : 0.7,
      data: data,
    };
  }

  function strategyNextData() {
    var el = document.querySelector('script#__NEXT_DATA__');
    if (!el) return null;
    var raw = el.textContent || '';
    var parsed = safeJSONParse(raw);
    if (!parsed) return null;
    // Drill to the most useful subtree if available.
    var page = parsed && parsed.props && parsed.props.pageProps;
    if (!page) {
      return { strategy: 'next_data', confidence: 0.7, data: parsed };
    }
    // pageProps with substantial nested content is almost always the page's
    // primary app data (markets list on Polymarket, products on a Shopify-
    // Next site, posts on a forum). Bump above json_ld's 0.95 so the
    // auto-picker prefers it over the typically-smaller schema.org metadata.
    // Threshold (1 KB serialized) excludes routing-only pageProps that some
    // marketing pages emit.
    var size = 0;
    try { size = JSON.stringify(page).length; } catch (e) {}
    return {
      strategy: 'next_data',
      confidence: size > 1024 ? 0.97 : 0.85,
      data: page,
    };
  }

  // Next.js App Router (RSC) — the new replacement for __NEXT_DATA__. The
  // page emits many <script> blocks of the form
  //   self.__next_f.push([1, "<key>:<value>\n<key>:<value>\n..."])
  // where each line is a Flight-protocol entry. We concatenate the string
  // payloads (chunk-boundary-agnostic by design — that's the protocol),
  // split into <key>:<value> records, and JSON-parse the values that are
  // plain data (skipping the "$S..." type-symbol lines and "I[id,...]"
  // module-reference lines that aren't content).
  //
  // Returns the parsed entries as a {key: value} dict. The key is the
  // chunk's reference id; values are the route/page state, server-action
  // returns, and prefetched data the page would otherwise hydrate from.
  function strategyRscPayload() {
    var pushRe = /__next_f\.push\(\s*\[\s*1\s*,\s*("(?:\\.|[^"\\])*")\s*\]\s*\)/g;
    var chunks = [];
    // First try the live DOM (works on sites that don't clean up after
    // hydration — e.g. Tailwind).
    var scripts = document.querySelectorAll('script');
    for (var i = 0; i < scripts.length; i++) {
      var t = scripts[i].textContent || '';
      if (t.indexOf('__next_f.push') < 0) continue;
      pushRe.lastIndex = 0;
      var m;
      while ((m = pushRe.exec(t)) !== null) {
        try { chunks.push(JSON.parse(m[1])); } catch (e) {}
      }
    }
    // Fallback: scan the raw HTML body if hydration scripts have removed
    // the inline `<script>` blocks (Next.js App Router does this — by the
    // time we extract, the live DOM has 0 scripts containing __next_f even
    // though the original body had dozens). The host function returns the
    // body of the most recent navigate.
    if (!chunks.length && typeof __host_raw_body === 'function') {
      var raw = __host_raw_body();
      if (raw) {
        pushRe.lastIndex = 0;
        var m2;
        while ((m2 = pushRe.exec(raw)) !== null) {
          try { chunks.push(JSON.parse(m2[1])); } catch (e) {}
        }
      }
    }
    if (!chunks.length) return null;

    var combined = chunks.join('');
    var entries = {};
    var lines = combined.split('\n');
    for (var j = 0; j < lines.length; j++) {
      var line = lines[j];
      if (!line) continue;
      var colon = line.indexOf(':');
      if (colon <= 0) continue;
      var key = line.substring(0, colon);
      var value = line.substring(colon + 1);
      if (!key || !value) continue;
      // Skip module references and type symbols — they're plumbing, not data:
      //   "$Sreact.fragment" — symbol reference
      //   I[id, [chunks], "name"] — client component module reference
      //   HL[...] — hint/preload directive
      var c0 = value.charAt(0);
      if (c0 === 'I' && value.charAt(1) === '[') continue;
      if (c0 === 'H' && (value.charAt(1) === 'L' || value.charAt(1) === 'B')) continue;
      if (c0 === '"' && value.charAt(1) === '$') continue;
      try {
        entries[key] = JSON.parse(value);
      } catch (e) { /* not parseable JSON — skip */ }
    }
    var keyCount = Object.keys(entries).length;
    if (keyCount === 0) return null;
    var size = 0;
    try { size = JSON.stringify(entries).length; } catch (e) {}
    // Confidence parallels next_data: substantial RSC data is the page's
    // primary state. Below next_data's 0.97 so a hybrid page (rare) prefers
    // pageProps. Above json_ld so SPAs without metadata still surface data.
    return {
      strategy: 'rsc_payload',
      confidence: size > 1024 ? 0.93 : 0.75,
      data: entries,
    };
  }

  function strategyNuxtData() {
    // Nuxt drops a global window.__NUXT__ that's a JS object literal. We
    // can't trivially read it without exec_scripts:true; check both the
    // raw script form (Nuxt also embeds it in a <script id=__NUXT_DATA__>
    // since Nuxt 3) and the runtime global.
    var el = document.querySelector('script#__NUXT_DATA__');
    if (el) {
      var raw = el.textContent || '';
      var parsed = safeJSONParse(raw);
      if (parsed) return { strategy: 'nuxt_data', confidence: 0.85, data: parsed };
    }
    if (typeof window !== 'undefined' && window.__NUXT__) {
      return { strategy: 'nuxt_data', confidence: 0.85, data: window.__NUXT__ };
    }
    return null;
  }

  function strategyOgMeta() {
    var metas = document.querySelectorAll('meta');
    if (!metas.length) return null;
    var out = {};
    var keys = 0;
    for (var i = 0; i < metas.length; i++) {
      var m = metas[i];
      var k = m.getAttribute('property') || m.getAttribute('name') || '';
      var v = m.getAttribute('content') || '';
      if (!k || !v) continue;
      // Keep only the high-signal namespaces.
      if (k.indexOf('og:') === 0 || k.indexOf('twitter:') === 0 ||
          k === 'description' || k === 'keywords' || k === 'author' ||
          k === 'article:published_time' || k === 'article:author') {
        out[k] = v;
        keys++;
      }
    }
    if (!keys) return null;
    var titleEl = document.querySelector('title');
    if (titleEl) out['_title'] = (titleEl.textContent || '').trim();
    var canonical = document.querySelector('link[rel=canonical]');
    if (canonical) out['_canonical'] = canonical.getAttribute('href');
    // Confidence scales with how many of the core fields are present.
    var hasTitle = out['og:title'] || out['twitter:title'] || out['_title'];
    var hasDesc = out['og:description'] || out['twitter:description'] || out['description'];
    var conf = 0.4;
    if (hasTitle && hasDesc) conf = 0.65;
    if (out['og:type'] === 'article' || out['og:type'] === 'product') conf = 0.75;
    return { strategy: 'og_meta', confidence: conf, data: out };
  }

  function strategyMicrodata() {
    var roots = document.querySelectorAll('[itemscope]');
    if (!roots.length) return null;
    function readItem(el) {
      var item = {};
      var typeAttr = el.getAttribute('itemtype');
      if (typeAttr) item['@type'] = typeAttr;
      // Walk descendants looking for itemprop, but stop descending when we
      // hit another itemscope (that's a nested item, captured separately).
      var stack = [].concat(el.childNodes || []);
      while (stack.length) {
        var node = stack.shift();
        if (!node || node.nodeType !== 1) continue;
        var prop = node.getAttribute('itemprop');
        if (prop) {
          var v;
          if (node.hasAttribute('itemscope')) {
            v = readItem(node);
          } else {
            var tag = (node.tagName || '').toLowerCase();
            v = node.getAttribute('content') || node.getAttribute('href') ||
                node.getAttribute('src') || node.getAttribute('datetime') ||
                (tag === 'meta' ? node.getAttribute('content') : '') ||
                (node.textContent || '').trim();
          }
          if (item[prop] === undefined) item[prop] = v;
          else if (Array.isArray(item[prop])) item[prop].push(v);
          else item[prop] = [item[prop], v];
        }
        if (!node.hasAttribute('itemscope')) {
          for (var i = 0; i < (node.childNodes || []).length; i++) {
            stack.push(node.childNodes[i]);
          }
        }
      }
      return item;
    }
    var items = [];
    for (var r = 0; r < roots.length; r++) {
      var root = roots[r];
      // Skip nested itemscopes (they'll be captured by their parent).
      var p = root.parentNode;
      var nested = false;
      while (p && p.nodeType === 1) {
        if (p.hasAttribute && p.hasAttribute('itemscope')) { nested = true; break; }
        p = p.parentNode;
      }
      if (!nested) items.push(readItem(root));
    }
    if (!items.length) return null;
    return {
      strategy: 'microdata',
      confidence: items.length > 1 ? 0.7 : 0.6,
      data: items.length === 1 ? items[0] : items,
    };
  }

  // Magento, Shopify, BigCommerce, et al. embed product/page data in
  // custom-typed <script> tags so their own client JS can consume it.
  // Common shapes:
  //   <script type="text/x-magento-init">{...}</script>     (Magento, often dozens per page)
  //   <script type="text/x-shopify-app">{...}</script>      (Shopify)
  //   <script type="application/vnd.shopify.product+json">  (newer Shopify)
  //   <script id="bc-product">{...}</script>                (BigCommerce)
  //
  // Generalized: any <script> whose `type` is not a JS variant AND whose
  // textContent parses as JSON. We collect them keyed by type, returning
  // a flat object the agent can iterate. This catches the SSR-but-
  // products-in-script class of pages that look "static" (full nav chrome
  // + filter UI + headings) but whose actual data lives in script tags.
  function strategyJsonInScript() {
    var scripts = document.querySelectorAll('script[type]');
    if (!scripts.length) return null;
    var collected = {}; // type -> [parsed blobs]
    var hits = 0;
    for (var i = 0; i < scripts.length; i++) {
      var s = scripts[i];
      var t = (s.getAttribute('type') || '').toLowerCase();
      // Skip pure JS — already-recognized JSON shapes are handled by
      // dedicated strategies (json_ld, next_data, nuxt_data, og_meta).
      if (!t || t === 'text/javascript' || t === 'module' ||
          t === 'application/javascript' ||
          t === 'application/ld+json') continue;
      // Only consider types that strongly imply JSON payload.
      var looksJson = t.indexOf('json') !== -1 ||
                      t.indexOf('x-magento') !== -1 ||
                      t.indexOf('x-shopify') !== -1 ||
                      t.indexOf('x-component') !== -1;
      if (!looksJson) continue;
      var raw = (s.textContent || '').trim();
      if (!raw || raw[0] !== '{' && raw[0] !== '[') continue;
      var parsed = safeJSONParse(raw);
      if (!parsed) continue;
      if (!collected[t]) collected[t] = [];
      collected[t].push(parsed);
      hits++;
    }
    if (!hits) return null;
    // Confidence rises with how many script types we picked up; a single
    // type with one blob is moderate, multiple types or many blobs is
    // high (signals a real SSR-with-JSON-config page like Magento).
    var typeCount = Object.keys(collected).length;
    var conf = typeCount > 1 ? 0.85 : (hits > 5 ? 0.75 : 0.6);
    return {
      strategy: 'json_in_script',
      confidence: conf,
      data: collected,
      hint: hits + ' JSON-bearing script(s) across ' + typeCount + ' type(s)',
    };
  }

  function strategyTextMain() {
    // Always last-resort. The Rust side already exposes text_main via RPC,
    // but we duplicate a thin version here so the extract pipeline can run
    // self-contained. Returns null if nothing meaningful.
    if (typeof __textMain === 'function') {
      var t = __textMain();
      if (t && t.length > 50) {
        return { strategy: 'text_main', confidence: 0.3, data: t };
      }
    }
    var body = document.body ? (document.body.textContent || '').trim() : '';
    if (body.length > 50) {
      return { strategy: 'text_main', confidence: 0.2, data: body };
    }
    return null;
  }

  // extract_table — pull a <table> into {headers, rows}. Headers come
  // from <thead><th>...</th></thead> if present, else the first <tr>'s
  // <th> cells. Each subsequent <tr>'s <td> cells become a row dict
  // keyed by header (or 'col_N' if no header for that column).
  globalThis.__extractTable = function (selector) {
    var table = document.querySelector(selector);
    if (!table) return null;
    var headers = [];
    var thead = table.querySelector('thead');
    var headerRow = thead ? thead.querySelector('tr') : null;
    if (!headerRow) {
      // Look for the first <tr> that has <th> cells.
      var trs = table.querySelectorAll('tr');
      for (var i = 0; i < trs.length; i++) {
        if (trs[i].querySelector('th')) { headerRow = trs[i]; break; }
      }
    }
    if (headerRow) {
      var hcells = headerRow.querySelectorAll('th');
      for (var hi = 0; hi < hcells.length; hi++) {
        headers.push((hcells[hi].textContent || '').trim());
      }
    }
    var rows = [];
    var bodyTrs = table.querySelectorAll('tbody tr');
    if (!bodyTrs.length) {
      bodyTrs = [];
      var allTrs = table.querySelectorAll('tr');
      for (var ti = 0; ti < allTrs.length; ti++) {
        if (allTrs[ti] !== headerRow) bodyTrs.push(allTrs[ti]);
      }
    }
    for (var r = 0; r < bodyTrs.length; r++) {
      var tds = bodyTrs[r].querySelectorAll('td');
      if (!tds.length) continue;
      var rowObj = {};
      for (var c = 0; c < tds.length; c++) {
        var key = headers[c] || ('col_' + c);
        rowObj[key] = (tds[c].textContent || '').trim();
      }
      rows.push(rowObj);
    }
    return { headers: headers, rows: rows, row_count: rows.length };
  };

  // extract_list — pull a repeated card pattern into [{...}, {...}].
  // `itemSelector` matches each card; `fields` maps field names to
  // sub-selectors. Field spec shapes:
  //   "css selector"          -> textContent of first match
  //   "css selector @attr"    -> value of `attr` on first match
  //   ["css selector", "@attr"] -> same, tuple form
  // If the sub-selector returns null, the field value is null.
  globalThis.__extractList = function (itemSelector, fields, limit) {
    limit = limit || 1000;
    var items = document.querySelectorAll(itemSelector);
    var out = [];
    var fieldNames = Object.keys(fields || {});
    for (var i = 0; i < items.length && i < limit; i++) {
      var item = items[i];
      var rec = {};
      for (var fi = 0; fi < fieldNames.length; fi++) {
        var name = fieldNames[fi];
        var spec = fields[name];
        var sel = null;
        var attr = null;
        if (typeof spec === 'string') {
          var m = spec.match(/^(.+?)\s*@(\S+)$/);
          if (m) { sel = m[1].trim(); attr = m[2]; }
          else { sel = spec; }
        } else if (Array.isArray(spec) && spec.length === 2) {
          sel = spec[0];
          attr = String(spec[1]).replace(/^@/, '');
        } else {
          rec[name] = null;
          continue;
        }
        var el = sel ? item.querySelector(sel) : item;
        if (!el) { rec[name] = null; continue; }
        if (attr) {
          rec[name] = el.getAttribute(attr);
        } else {
          rec[name] = (el.textContent || '').trim();
        }
      }
      out.push(rec);
    }
    return out;
  };

  globalThis.__extract = function (opts) {
    opts = opts || {};
    var requested = opts.strategy; // optional: force a specific strategy
    var all = [
      ['json_ld', strategyJsonLd],
      ['next_data', strategyNextData],
      ['rsc_payload', strategyRscPayload],         // Next.js App Router
      ['nuxt_data', strategyNuxtData],
      ['json_in_script', strategyJsonInScript],   // Magento, Shopify, etc.
      ['og_meta', strategyOgMeta],
      ['microdata', strategyMicrodata],
      ['text_main', strategyTextMain],
    ];
    if (requested) {
      for (var i = 0; i < all.length; i++) {
        if (all[i][0] === requested) {
          var r = all[i][1]();
          return r || { strategy: requested, confidence: 0, data: null,
                        hint: 'requested strategy returned no data' };
        }
      }
      return { strategy: requested, confidence: 0, data: null,
               hint: 'unknown strategy ' + requested };
    }
    var tried = [];
    var best = null;
    var hits = [];  // strategies with confidence >= 0.5, full data carried
    for (var j = 0; j < all.length; j++) {
      var name = all[j][0], fn = all[j][1];
      try {
        var res = fn();
        tried.push({ strategy: name, confidence: res ? res.confidence : 0,
                     hit: !!res });
        if (res && (!best || res.confidence > best.confidence)) best = res;
        if (res && res.confidence >= 0.5) {
          hits.push({ strategy: name, confidence: res.confidence, data: res.data });
        }
      } catch (e) {
        tried.push({ strategy: name, confidence: 0, hit: false,
                     error: String(e && e.message || e) });
      }
    }
    if (!best) return { strategy: 'none', confidence: 0, data: null, tried: tried };
    // Sort hits by confidence desc; cap at 5 to bound payload size on pages
    // where many strategies hit (Polymarket: json_ld + next_data +
    // json_in_script + og_meta all return data).
    hits.sort(function(a, b) { return b.confidence - a.confidence; });
    if (hits.length > 5) hits.length = 5;
    best.tried = tried;
    best.all_hits = hits;
    return best;
  };
})();
