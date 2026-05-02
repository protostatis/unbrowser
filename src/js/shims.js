// Passive browser-API shims for the embedded QuickJS sandbox.
// Provides globals that page scripts reference at parse/init time so they
// don't crash on missing names. Mostly stubs — they don't *do* anything
// (no event loop, no fetch yet) — but they exist with realistic shapes.
//
// Coherent with our Chrome 131 TLS+H2 fingerprint: navigator.userAgent,
// navigator.platform, screen.*, etc. all match a Chrome 131 macOS session
// so a fingerprinter that combines TLS + JS-environment signals doesn't
// see a contradiction.
//
// What's NOT here yet (will need host hooks + an event loop):
//   - setTimeout / setInterval / clearTimeout / clearInterval
//   - fetch / XMLHttpRequest / Headers / Request / Response
//   - WebSocket
//   - actual Promise microtask scheduling for queueMicrotask
// Page scripts that reference these by name still parse fine; they'll
// throw at call time, which the eval method surfaces clearly.

(function() {

  // ---- window / self -----------------------------------------------------
  globalThis.window = globalThis;
  globalThis.self = globalThis;
  globalThis.top = globalThis;
  globalThis.parent = globalThis;
  globalThis.frames = [];

  // ---- location ----------------------------------------------------------
  // Updated by the host after each navigate via globalThis.__setLocation(url).
  var _location = {
    href: 'about:blank',
    protocol: 'about:',
    host: '',
    hostname: '',
    port: '',
    pathname: 'blank',
    search: '',
    hash: '',
    origin: 'null',
    assign: function(url) { _location.href = url; },
    replace: function(url) { _location.href = url; },
    reload: function() {},
    toString: function() { return _location.href; },
  };
  globalThis.__setLocation = function(url) {
    try {
      var m = url && url.match(/^(https?:)\/\/([^/:]+)(:\d+)?([^?#]*)(\?[^#]*)?(#.*)?$/);
      if (m) {
        _location.protocol = m[1];
        _location.hostname = m[2];
        _location.port = m[3] ? m[3].slice(1) : '';
        _location.host = m[2] + (m[3] || '');
        _location.pathname = m[4] || '/';
        _location.search = m[5] || '';
        _location.hash = m[6] || '';
        _location.origin = m[1] + '//' + _location.host;
        _location.href = url;
      } else if (url) {
        _location.href = url;
      }
    } catch (e) { /* swallow */ }
  };
  globalThis.location = _location;

  // ---- navigator (Chrome 131 on macOS — coherent with TLS fingerprint) ----
  globalThis.navigator = {
    userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36',
    appName: 'Netscape',
    appVersion: '5.0 (Macintosh)',
    appCodeName: 'Mozilla',
    product: 'Gecko',
    productSub: '20030107',
    vendor: 'Google Inc.',
    vendorSub: '',
    language: 'en-US',
    languages: ['en-US', 'en'],
    platform: 'MacIntel',
    cookieEnabled: true,
    onLine: true,
    doNotTrack: null,
    hardwareConcurrency: 10,
    deviceMemory: 8,
    maxTouchPoints: 0,
    pdfViewerEnabled: true,
    webdriver: undefined,
    plugins: { length: 0, item: function() { return null; }, namedItem: function() { return null; }, refresh: function() {} },
    mimeTypes: { length: 0, item: function() { return null; }, namedItem: function() { return null; } },
    userAgentData: {
      brands: [
        { brand: 'Chromium', version: '131' },
        { brand: 'Google Chrome', version: '131' },
        { brand: 'Not_A Brand', version: '24' },
      ],
      mobile: false,
      platform: 'macOS',
    },
    sendBeacon: function() { return true; },
    javaEnabled: function() { return false; },
    permissions: { query: function() { return Promise.resolve({ state: 'prompt' }); } },
    clipboard: { writeText: function() { return Promise.resolve(); }, readText: function() { return Promise.resolve(''); } },
  };

  // ---- screen (a common Mac retina laptop) -------------------------------
  globalThis.screen = {
    availWidth: 1440,
    availHeight: 875,
    width: 1440,
    height: 900,
    colorDepth: 24,
    pixelDepth: 24,
    availLeft: 0,
    availTop: 25,
    orientation: { type: 'landscape-primary', angle: 0, addEventListener: function() {}, removeEventListener: function() {} },
  };
  globalThis.devicePixelRatio = 2;
  globalThis.innerWidth = 1440;
  globalThis.innerHeight = 800;
  globalThis.outerWidth = 1440;
  globalThis.outerHeight = 900;
  globalThis.scrollX = 0;
  globalThis.scrollY = 0;
  globalThis.pageXOffset = 0;
  globalThis.pageYOffset = 0;

  // ---- history ----------------------------------------------------------
  globalThis.history = {
    length: 1,
    state: null,
    scrollRestoration: 'auto',
    pushState: function(state, title, url) { if (url) __setLocation(url); },
    replaceState: function(state, title, url) { if (url) __setLocation(url); },
    go: function() {},
    back: function() {},
    forward: function() {},
  };

  // ---- localStorage / sessionStorage (in-memory) ------------------------
  function MemoryStorage() { this._data = {}; }
  MemoryStorage.prototype.getItem = function(k) { return Object.prototype.hasOwnProperty.call(this._data, k) ? this._data[k] : null; };
  MemoryStorage.prototype.setItem = function(k, v) { this._data[k] = String(v); };
  MemoryStorage.prototype.removeItem = function(k) { delete this._data[k]; };
  MemoryStorage.prototype.clear = function() { this._data = {}; };
  MemoryStorage.prototype.key = function(i) { return Object.keys(this._data)[i] || null; };
  Object.defineProperty(MemoryStorage.prototype, 'length', { get: function() { return Object.keys(this._data).length; } });
  globalThis.localStorage = new MemoryStorage();
  globalThis.sessionStorage = new MemoryStorage();

  // ---- performance ------------------------------------------------------
  var _start = Date.now();
  globalThis.performance = {
    now: function() { return Date.now() - _start; },
    timeOrigin: _start,
    mark: function() {},
    measure: function() {},
    clearMarks: function() {},
    clearMeasures: function() {},
    getEntries: function() { return []; },
    getEntriesByName: function() { return []; },
    getEntriesByType: function() { return []; },
    timing: { navigationStart: _start, fetchStart: _start, requestStart: _start, responseEnd: _start },
    navigation: { type: 0, redirectCount: 0 },
  };

  // ---- Timer subsystem (setTimeout / setInterval / rAF / rIC) -----------
  //
  // Pull-based event loop: timer callbacks live in JS-side _timers; the
  // host's `settle` method polls `__pumpTimers()` after sleeping to the
  // next deadline. This means timers DO NOT fire during synchronous JS
  // execution — they fire when the host explicitly drains the loop.
  // For typical page-init use (DOMContentLoaded handlers + a setTimeout(fn,0)
  // for "next tick"), call settle() once after seeding the DOM.
  var _timers = {};
  var _nextTimerId = 1;

  globalThis.setTimeout = function(cb, ms) {
    if (typeof cb !== 'function') return 0;
    ms = Math.max(0, Number(ms) || 0);
    var id = _nextTimerId++;
    _timers[id] = { cb: cb, deadline: Date.now() + ms, interval: null };
    return id;
  };
  globalThis.setInterval = function(cb, ms) {
    if (typeof cb !== 'function') return 0;
    ms = Math.max(1, Number(ms) || 1);
    var id = _nextTimerId++;
    _timers[id] = { cb: cb, deadline: Date.now() + ms, interval: ms };
    return id;
  };
  globalThis.clearTimeout = function(id) { delete _timers[id]; };
  globalThis.clearInterval = function(id) { delete _timers[id]; };

  globalThis.requestAnimationFrame = function(cb) { return setTimeout(cb, 16); };
  globalThis.cancelAnimationFrame  = function(id) { clearTimeout(id); };
  globalThis.requestIdleCallback   = function(cb) { return setTimeout(cb, 1); };
  globalThis.cancelIdleCallback    = function(id) { clearTimeout(id); };
  globalThis.queueMicrotask        = function(cb) { Promise.resolve().then(cb); };

  // ---- fetch() — bridges to Rust via __host_fetch_send + __host_drain_fetches.
  //
  // Pull-based: fetch() registers a pending Promise + sends the request to the
  // Rust worker thread. The worker performs the rquest async, queues the
  // response. settle() periodically calls __pollFetches() which drains and
  // resolves matching Promises. Routes through the same rquest::Client as
  // navigate so cookies + Chrome 131 TLS fingerprint stay coherent.
  var _pendingFetches = {};
  var _nextFetchId = 1;

  globalThis.fetch = function(input, init) {
    init = init || {};
    var url = (typeof input === 'string') ? input : (input && input.url ? input.url : String(input));
    var method = (init.method || 'GET').toUpperCase();
    var headers = {};
    if (init.headers) {
      if (init.headers instanceof Headers) {
        init.headers.forEach(function(v, k) { headers[k] = v; });
      } else if (typeof init.headers === 'object') {
        for (var k in init.headers) headers[k] = String(init.headers[k]);
      }
    }
    var body = init.body || '';
    if (typeof body !== 'string') {
      try { body = JSON.stringify(body); } catch (e) { body = String(body); }
    }
    var id = _nextFetchId++;
    return new Promise(function(resolve, reject) {
      _pendingFetches[id] = { resolve: resolve, reject: reject, url: url };
      if (typeof __host_fetch_send !== 'function') {
        delete _pendingFetches[id];
        reject(new Error('fetch host binding not available'));
        return;
      }
      try {
        __host_fetch_send(id, method, url, JSON.stringify(headers), body);
      } catch (e) {
        delete _pendingFetches[id];
        reject(e);
      }
    });
  };

  // Builds a Response-like object from the host's drained payload.
  function buildResponse(pending, raw) {
    var status = raw.status || 0;
    var bodyText = raw.body || '';
    return {
      ok: status >= 200 && status < 300,
      status: status,
      statusText: '',
      url: pending.url,
      redirected: false,
      type: 'basic',
      bodyUsed: false,
      headers: new Headers(raw.headers || {}),
      text: function() { this.bodyUsed = true; return Promise.resolve(bodyText); },
      json: function() {
        this.bodyUsed = true;
        try { return Promise.resolve(JSON.parse(bodyText)); }
        catch (e) { return Promise.reject(e); }
      },
      arrayBuffer: function() { this.bodyUsed = true; return Promise.resolve(new ArrayBuffer(0)); },
      blob: function() { this.bodyUsed = true; return Promise.resolve(new Blob([bodyText])); },
      clone: function() { return this; }
    };
  }

  globalThis.__pollFetches = function() {
    if (typeof __host_drain_fetches !== 'function') return 0;
    var raw;
    try { raw = __host_drain_fetches(); } catch (e) { return 0; }
    if (!raw || raw === '[]') return 0;
    var results;
    try { results = JSON.parse(raw); } catch (e) { return 0; }
    if (!Array.isArray(results)) return 0;
    var resolved = 0;
    for (var i = 0; i < results.length; i++) {
      var r = results[i];
      var pending = _pendingFetches[r.id];
      if (!pending) continue;
      delete _pendingFetches[r.id];
      if (r.error) {
        pending.reject(new Error(r.error));
      } else {
        pending.resolve(buildResponse(pending, r));
      }
      resolved++;
    }
    return resolved;
  };

  globalThis.__pendingFetches = function() {
    return Object.keys(_pendingFetches).length;
  };

  // ---- Host-facing event-loop helpers (used by Rust settle) -------------
  globalThis.__pendingTimers = function() {
    return Object.keys(_timers).length;
  };
  globalThis.__nextTimerDeadline = function() {
    var min = Infinity;
    for (var id in _timers) if (_timers[id].deadline < min) min = _timers[id].deadline;
    return min === Infinity ? null : min;
  };
  globalThis.__pumpTimers = function() {
    var now = Date.now();
    var fired = 0;
    var ids = Object.keys(_timers);  // snapshot — callbacks may mutate
    for (var i = 0; i < ids.length; i++) {
      var t = _timers[ids[i]];
      if (!t || t.deadline > now) continue;
      try { t.cb(); } catch (e) { /* swallow — surface via separate error log if needed */ }
      if (!_timers[ids[i]]) continue;  // cb may have cleared itself
      if (t.interval) {
        t.deadline = Date.now() + t.interval;
      } else {
        delete _timers[ids[i]];
      }
      fired++;
    }
    return fired;
  };

  // ---- getComputedStyle / matchMedia (stubs) ----------------------------
  globalThis.getComputedStyle = function(el) {
    return el && el.style ? el.style : { getPropertyValue: function() { return ''; } };
  };
  globalThis.matchMedia = function(query) {
    return {
      matches: false,
      media: query,
      onchange: null,
      addListener: function() {},
      removeListener: function() {},
      addEventListener: function() {},
      removeEventListener: function() {},
      dispatchEvent: function() { return false; },
    };
  };

  // ---- Observers (content-positive: fire callbacks so page logic proceeds)
  //
  // Previous implementation was NoopObserver — never fired callbacks.
  // That broke any page that gates rendering on observer events:
  //   - lazy-load grids waiting for IntersectionObserver(isIntersecting=true)
  //   - hydration paths waiting for MutationObserver to confirm DOM changed
  //   - layout-driven UI waiting for ResizeObserver
  // The page would settle with empty content because the "I am ready"
  // callback never ran.
  //
  // Content-positive defaults:
  //   IntersectionObserver: fire once per observed target with
  //     isIntersecting=true (we don't render — everything is "in view").
  //     Unlocks lazy-load.
  //   ResizeObserver: fire once per observed target with viewport-ish
  //     dimensions. Unlocks layout-conditioned content.
  //   MutationObserver: actually notice DOM mutations. Records routed
  //     from dom.js's recordMutation via __notifyMutationObservers,
  //     converted to MutationRecord shape, fired async via microtask.
  //   PerformanceObserver: stays no-op — we don't generate perf entries
  //     and pages don't gate content on them.
  //
  // Fire timing: queueMicrotask matches browser semantics (callbacks
  // are not synchronous w.r.t. the observe()/mutation call site).

  // Synthetic dimensions for the viewport — match the screen dims set
  // earlier in this file so a page reading both stays coherent.
  var VIEWPORT_W = 1280;
  var VIEWPORT_H = 800;

  function syntheticBoundingRect() {
    return {
      x: 0, y: 0, width: VIEWPORT_W, height: VIEWPORT_H,
      top: 0, right: VIEWPORT_W, bottom: VIEWPORT_H, left: 0,
      toJSON: function() { return this; },
    };
  }

  function IntersectionObserver(callback, options) {
    this._callback = callback;
    this._options = options || {};
    this._observed = [];
  }
  IntersectionObserver.prototype.observe = function(target) {
    if (this._observed.indexOf(target) === -1) this._observed.push(target);
    var cb = this._callback;
    var self = this;
    queueMicrotask(function() {
      try {
        cb([{
          isIntersecting: true,
          intersectionRatio: 1,
          target: target,
          time: (typeof performance !== 'undefined' && performance.now) ? performance.now() : Date.now(),
          boundingClientRect: syntheticBoundingRect(),
          intersectionRect: syntheticBoundingRect(),
          rootBounds: syntheticBoundingRect(),
        }], self);
      } catch (e) {}
    });
  };
  IntersectionObserver.prototype.unobserve = function(target) {
    var i = this._observed.indexOf(target);
    if (i !== -1) this._observed.splice(i, 1);
  };
  IntersectionObserver.prototype.disconnect = function() { this._observed = []; };
  IntersectionObserver.prototype.takeRecords = function() { return []; };
  globalThis.IntersectionObserver = IntersectionObserver;

  function ResizeObserver(callback) {
    this._callback = callback;
    this._observed = [];
  }
  ResizeObserver.prototype.observe = function(target) {
    if (this._observed.indexOf(target) === -1) this._observed.push(target);
    var cb = this._callback;
    var self = this;
    queueMicrotask(function() {
      try {
        cb([{
          target: target,
          contentRect: syntheticBoundingRect(),
          borderBoxSize: [{ inlineSize: VIEWPORT_W, blockSize: VIEWPORT_H }],
          contentBoxSize: [{ inlineSize: VIEWPORT_W, blockSize: VIEWPORT_H }],
          devicePixelContentBoxSize: [{ inlineSize: VIEWPORT_W, blockSize: VIEWPORT_H }],
        }], self);
      } catch (e) {}
    });
  };
  ResizeObserver.prototype.unobserve = function(target) {
    var i = this._observed.indexOf(target);
    if (i !== -1) this._observed.splice(i, 1);
  };
  ResizeObserver.prototype.disconnect = function() { this._observed = []; };
  globalThis.ResizeObserver = ResizeObserver;

  // MutationObserver — notified by dom.js's recordMutation via
  // __notifyMutationObservers. Each observer keeps a queue and fires
  // its callback once per microtask checkpoint (matches browser
  // batching semantics).
  //
  // For v1 we don't filter by observed-target subtree — every active
  // observer gets every mutation. This is over-firing but practically
  // works because page code typically filters records itself ("did
  // this specific node appear"). Subtree filtering is a follow-up.
  var __activeMutationObservers = [];

  function MutationObserver(callback) {
    this._callback = callback;
    this._records = [];
    this._observed = [];   // [{ target, options }]
    this._scheduled = false;
  }
  MutationObserver.prototype.observe = function(target, options) {
    this._observed.push({ target: target, options: options || {} });
    if (__activeMutationObservers.indexOf(this) === -1) {
      __activeMutationObservers.push(this);
    }
  };
  MutationObserver.prototype.disconnect = function() {
    this._observed = [];
    var i = __activeMutationObservers.indexOf(this);
    if (i !== -1) __activeMutationObservers.splice(i, 1);
  };
  MutationObserver.prototype.takeRecords = function() {
    var r = this._records;
    this._records = [];
    return r;
  };
  MutationObserver.prototype._enqueue = function(record) {
    this._records.push(record);
    if (this._scheduled) return;
    this._scheduled = true;
    var self = this;
    queueMicrotask(function() {
      self._scheduled = false;
      var records = self._records;
      self._records = [];
      if (records.length === 0) return;
      try { self._callback(records, self); } catch (e) {}
    });
  };
  globalThis.MutationObserver = MutationObserver;

  // Convert a dom.js internal mutation to a WHATWG MutationRecord.
  // Returns null for mutation types that don't map to a record.
  function toMutationRecord(m) {
    var byId = (typeof __nodeById === 'function') ? __nodeById : function() { return null; };
    if (m.type === 'appendChild' || m.type === 'insertBefore') {
      var added = byId(m.childId);
      return {
        type: 'childList',
        target: byId(m.parentId),
        addedNodes: added ? [added] : [],
        removedNodes: [],
        previousSibling: null,
        nextSibling: null,
        attributeName: null,
        attributeNamespace: null,
        oldValue: null,
      };
    }
    if (m.type === 'removeChild') {
      var removed = byId(m.childId);
      return {
        type: 'childList',
        target: byId(m.parentId),
        addedNodes: [],
        removedNodes: removed ? [removed] : [],
        previousSibling: null,
        nextSibling: null,
        attributeName: null,
        attributeNamespace: null,
        oldValue: null,
      };
    }
    if (m.type === 'setAttribute' || m.type === 'removeAttribute') {
      return {
        type: 'attributes',
        target: byId(m.id),
        addedNodes: [],
        removedNodes: [],
        previousSibling: null,
        nextSibling: null,
        attributeName: m.name || null,
        attributeNamespace: null,
        oldValue: null,
      };
    }
    if (m.type === 'setTextContent') {
      return {
        type: 'characterData',
        target: byId(m.id),
        addedNodes: [],
        removedNodes: [],
        previousSibling: null,
        nextSibling: null,
        attributeName: null,
        attributeNamespace: null,
        oldValue: null,
      };
    }
    // setStyle isn't a MutationRecord type; modern browsers expose this
    // via the style attribute observer. We skip — pages observing style
    // changes are rare and we'd need to model attribute mutations on
    // 'style' specifically. Follow-up if it shows up in test data.
    return null;
  }

  globalThis.__notifyMutationObservers = function(internalMutation) {
    if (__activeMutationObservers.length === 0) return;
    var record = toMutationRecord(internalMutation);
    if (!record) return;
    for (var i = 0; i < __activeMutationObservers.length; i++) {
      __activeMutationObservers[i]._enqueue(record);
    }
  };

  // PerformanceObserver stays no-op — pages don't gate content delivery
  // on perf entries, and we don't generate any.
  function NoopObserver() {}
  NoopObserver.prototype.observe = function() {};
  NoopObserver.prototype.unobserve = function() {};
  NoopObserver.prototype.disconnect = function() {};
  NoopObserver.prototype.takeRecords = function() { return []; };
  globalThis.PerformanceObserver = NoopObserver;

  // ---- atob / btoa ------------------------------------------------------
  var B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  globalThis.atob = function(str) {
    var output = '';
    str = String(str).replace(/=+$/, '');
    while (str.length % 4) str += '=';
    for (var i = 0; i < str.length; i += 4) {
      var a = B64.indexOf(str[i]),     b = B64.indexOf(str[i+1]);
      var c = B64.indexOf(str[i+2]),   d = B64.indexOf(str[i+3]);
      if (a < 0) a = 0; if (b < 0) b = 0;
      if (c < 0) c = 0; if (d < 0) d = 0;
      var bits = (a << 18) | (b << 12) | (c << 6) | d;
      output += String.fromCharCode((bits >> 16) & 0xff);
      if (str[i+2] !== '=') output += String.fromCharCode((bits >> 8) & 0xff);
      if (str[i+3] !== '=') output += String.fromCharCode(bits & 0xff);
    }
    return output;
  };
  globalThis.btoa = function(str) {
    var output = '';
    str = String(str);
    for (var i = 0; i < str.length; i += 3) {
      var a = str.charCodeAt(i), b = str.charCodeAt(i+1) || 0, c = str.charCodeAt(i+2) || 0;
      var bits = (a << 16) | (b << 8) | c;
      output += B64[(bits >> 18) & 63] + B64[(bits >> 12) & 63];
      output += (i + 1 < str.length) ? B64[(bits >> 6) & 63] : '=';
      output += (i + 2 < str.length) ? B64[bits & 63] : '=';
    }
    return output;
  };

  // ---- console ---------------------------------------------------------
  // QuickJS doesn't ship console by default; page scripts use it constantly
  // (Ember, React, Vue, jQuery, …). Default to no-op so script execution
  // doesn't crash. Could route to host stderr later if visibility helps debugging.
  globalThis.console = globalThis.console || {
    log:        function() {},
    warn:       function() {},
    error:      function() {},
    info:       function() {},
    debug:      function() {},
    trace:      function() {},
    table:      function() {},
    group:      function() {},
    groupCollapsed: function() {},
    groupEnd:   function() {},
    time:       function() {},
    timeEnd:    function() {},
    timeLog:    function() {},
    assert:     function() {},
    dir:        function() {},
    dirxml:     function() {},
    count:      function() {},
    countReset: function() {},
    clear:      function() {},
  };

  // ---- crypto -----------------------------------------------------------
  // getRandomValues uses Math.random — fine for non-security uses (which is
  // most of what page scripts do with it). subtle.digest stub returns an
  // empty hash — should be replaced when there's a real reason.
  globalThis.crypto = globalThis.crypto || {
    getRandomValues: function(arr) {
      for (var i = 0; i < arr.length; i++) arr[i] = Math.floor(Math.random() * 256);
      return arr;
    },
    subtle: { digest: function() { return Promise.resolve(new ArrayBuffer(32)); } },
    randomUUID: function() {
      return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function(c) {
        var r = (Math.random() * 16) | 0;
        return (c === 'x' ? r : ((r & 0x3) | 0x8)).toString(16);
      });
    },
  };

  // ---- DOMContentLoaded / load helpers ----------------------------------
  // Called by the host after page-script execution settles (Phase 5).
  // Page scripts that use addEventListener('DOMContentLoaded', ...) or
  // window.onload will fire when these are invoked.
  var _windowListeners = {};
  globalThis.window.addEventListener = function(type, fn) {
    if (!_windowListeners[type]) _windowListeners[type] = [];
    _windowListeners[type].push(fn);
  };
  globalThis.window.removeEventListener = function(type, fn) {
    if (!_windowListeners[type]) return;
    _windowListeners[type] = _windowListeners[type].filter(function(f) { return f !== fn; });
  };
  globalThis.window.dispatchEvent = function(event) {
    var listeners = _windowListeners[event.type] || [];
    for (var i = 0; i < listeners.length; i++) listeners[i](event);
    return !event.defaultPrevented;
  };
  globalThis.__fireDOMContentLoaded = function() {
    document.readyState = 'interactive';
    // Real browsers bubble DOMContentLoaded document → window; our DOM doesn't
    // link document to window, so we dispatch on both. Frameworks (Ember,
    // jQuery, …) register on either, sometimes both.
    document.dispatchEvent(new Event('readystatechange'));
    var dcl = new Event('DOMContentLoaded', { bubbles: true });
    document.dispatchEvent(dcl);
    window.dispatchEvent(dcl);
    if (typeof document.onreadystatechange === 'function') document.onreadystatechange();
  };
  globalThis.__fireLoad = function() {
    document.readyState = 'complete';
    document.dispatchEvent(new Event('readystatechange'));
    var ev = new Event('load', { bubbles: false });
    document.dispatchEvent(ev);
    window.dispatchEvent(ev);
    if (typeof window.onload === 'function') window.onload(ev);
    if (typeof document.onreadystatechange === 'function') document.onreadystatechange();
  };
  globalThis.window.onload = null;
  globalThis.window.onunload = null;
  globalThis.window.onbeforeunload = null;

  // ---- Misc utilities / crash-prevention stubs --------------------------
  globalThis.structuredClone = function(obj) { return JSON.parse(JSON.stringify(obj)); };
  globalThis.alert = function(msg) { /* no-op */ };
  globalThis.confirm = function() { return false; };
  globalThis.prompt = function() { return null; };
  globalThis.scroll = function() {};
  globalThis.scrollTo = function() {};
  globalThis.scrollBy = function() {};
  globalThis.focus = function() {};
  globalThis.blur = function() {};
  globalThis.print = function() {};
  globalThis.open = function() { return null; };
  globalThis.close = function() {};
  globalThis.stop = function() {};

  // Headers/Request/Response/Blob/File minimal stubs so `typeof Headers`
  // doesn't break feature-detection code. They're not functional yet.
  globalThis.Headers = globalThis.Headers || function(init) {
    this._h = {};
    if (init && typeof init === 'object') {
      for (var k in init) this._h[k.toLowerCase()] = init[k];
    }
  };
  if (globalThis.Headers && !globalThis.Headers.prototype.get) {
    globalThis.Headers.prototype.get = function(k) { return this._h[String(k).toLowerCase()] || null; };
    globalThis.Headers.prototype.set = function(k, v) { this._h[String(k).toLowerCase()] = String(v); };
    globalThis.Headers.prototype.has = function(k) { return String(k).toLowerCase() in this._h; };
    globalThis.Headers.prototype.delete = function(k) { delete this._h[String(k).toLowerCase()]; };
    globalThis.Headers.prototype.append = function(k, v) {
      var key = String(k).toLowerCase();
      this._h[key] = (this._h[key] ? this._h[key] + ', ' : '') + String(v);
    };
    globalThis.Headers.prototype.forEach = function(cb) {
      for (var k in this._h) cb(this._h[k], k, this);
    };
  }
  globalThis.Blob = globalThis.Blob || function() {};
  globalThis.File = globalThis.File || function() {};
  globalThis.FormData = globalThis.FormData || function() {
    this._d = [];
    this.append = function(k, v) { this._d.push([k, v]); };
    this.get = function(k) { for (var i = 0; i < this._d.length; i++) if (this._d[i][0] === k) return this._d[i][1]; return null; };
  };
  globalThis.AbortController = globalThis.AbortController || function() {
    this.signal = { aborted: false, addEventListener: function() {}, removeEventListener: function() {} };
    this.abort = function() { this.signal.aborted = true; };
  };

  // ---- XMLHttpRequest — wraps fetch() ------------------------------------
  // Older jQuery, GA legacy, many polyfilled libs use XHR. Implementing it
  // on top of fetch keeps cookies + TLS coherence and avoids a second
  // host-binding layer.
  globalThis.XMLHttpRequest = function() {
    var self = this;
    this.readyState = 0;
    this.status = 0;
    this.statusText = '';
    this.responseText = '';
    this.response = '';
    this.responseURL = '';
    this.responseType = '';
    this.timeout = 0;
    this.withCredentials = false;
    this.onreadystatechange = null;
    this.onload = null;
    this.onerror = null;
    this.onabort = null;
    this.onloadstart = null;
    this.onloadend = null;
    this.onprogress = null;
    this.ontimeout = null;
    self._method = 'GET';
    self._url = '';
    self._async = true;
    self._headers = {};
    self._respHeaders = {};
    self._aborted = false;
    self._listeners = {};

    function fire(type) {
      var handler = self['on' + type];
      if (typeof handler === 'function') { try { handler.call(self); } catch (e) {} }
      var list = self._listeners[type] || [];
      for (var i = 0; i < list.length; i++) { try { list[i].call(self); } catch (e) {} }
    }

    this.open = function(method, url, async) {
      self._method = String(method || 'GET').toUpperCase();
      self._url = String(url || '');
      self._async = (async !== false);
      self.readyState = 1;
      fire('readystatechange');
    };
    this.setRequestHeader = function(k, v) { self._headers[String(k)] = String(v); };
    this.getResponseHeader = function(k) {
      var key = String(k || '').toLowerCase();
      return self._respHeaders[key] || null;
    };
    this.getAllResponseHeaders = function() {
      var out = '';
      for (var k in self._respHeaders) out += k + ': ' + self._respHeaders[k] + '\r\n';
      return out;
    };
    this.overrideMimeType = function() {};
    this.send = function(body) {
      self.readyState = 2;
      fire('readystatechange');
      fire('loadstart');
      var init = { method: self._method, headers: self._headers };
      if (body !== undefined && body !== null && self._method !== 'GET' && self._method !== 'HEAD') {
        init.body = body;
      }
      fetch(self._url, init).then(function(resp) {
        if (self._aborted) return;
        self.status = resp.status;
        self.statusText = resp.statusText || '';
        self.responseURL = resp.url || self._url;
        self._respHeaders = {};
        if (resp.headers && resp.headers.forEach) {
          resp.headers.forEach(function(v, k) { self._respHeaders[String(k).toLowerCase()] = v; });
        }
        self.readyState = 3;
        fire('readystatechange');
        return resp.text();
      }).then(function(text) {
        if (self._aborted) return;
        self.responseText = text || '';
        if (self.responseType === 'json') {
          try { self.response = JSON.parse(self.responseText); } catch (e) { self.response = null; }
        } else {
          self.response = self.responseText;
        }
        self.readyState = 4;
        fire('readystatechange');
        fire('load');
        fire('loadend');
      }).catch(function(err) {
        if (self._aborted) return;
        self.readyState = 4;
        fire('readystatechange');
        fire('error');
        fire('loadend');
      });
    };
    this.abort = function() {
      self._aborted = true;
      self.readyState = 4;
      fire('readystatechange');
      fire('abort');
      fire('loadend');
    };
    this.addEventListener = function(type, fn) {
      if (!self._listeners[type]) self._listeners[type] = [];
      self._listeners[type].push(fn);
    };
    this.removeEventListener = function(type, fn) {
      if (!self._listeners[type]) return;
      self._listeners[type] = self._listeners[type].filter(function(f) { return f !== fn; });
    };
    this.dispatchEvent = function() { return true; };
  };
  globalThis.XMLHttpRequest.UNSENT = 0;
  globalThis.XMLHttpRequest.OPENED = 1;
  globalThis.XMLHttpRequest.HEADERS_RECEIVED = 2;
  globalThis.XMLHttpRequest.LOADING = 3;
  globalThis.XMLHttpRequest.DONE = 4;

  // ---- Analytics / tracking globals (stub so pages don't crash) ---------
  globalThis.ga = globalThis.ga || function() {};
  globalThis.gtag = globalThis.gtag || function() {};
  globalThis._gaq = globalThis._gaq || { push: function() {} };
  globalThis.dataLayer = globalThis.dataLayer || [];

  // Mark shims as installed so callers can feature-detect.
  globalThis.__shims_installed = true;

})();
