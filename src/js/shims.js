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

  // ---- Observers (no-op, prevent crashes) -------------------------------
  function NoopObserver() {}
  NoopObserver.prototype.observe = function() {};
  NoopObserver.prototype.unobserve = function() {};
  NoopObserver.prototype.disconnect = function() {};
  NoopObserver.prototype.takeRecords = function() { return []; };
  globalThis.ResizeObserver = NoopObserver;
  globalThis.IntersectionObserver = NoopObserver;
  globalThis.MutationObserver = NoopObserver;
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
    document.dispatchEvent(new Event('DOMContentLoaded'));
  };
  globalThis.__fireLoad = function() {
    document.readyState = 'complete';
    if (typeof window.onload === 'function') window.onload(new Event('load'));
    window.dispatchEvent(new Event('load'));
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

  // Mark shims as installed so callers can feature-detect.
  globalThis.__shims_installed = true;

})();
