// Ported from sky-search/client/wasm/dom.js — un-wrapped from template literal.
// Defines document, Element, Node, ClassList, querySelector, etc. inside the QuickJS sandbox.
// Seeds the DOM via globalThis.__seedDOM(tree) where tree is the JSON shape produced by Rust.

(function() {
  // --- Mutation Recording ---
  var __mutations = [];
  var __nextId = 1;
  // id → node registry, populated in Node constructor. Lets external
  // consumers (notably the MutationObserver shim in shims.js) resolve
  // a mutation's parentId/childId/id back to the actual Node object so
  // MutationRecord.target / addedNodes / removedNodes are populated.
  // Cleared on __seedDOM. Plain object — Map would be cleaner but we
  // already use object-as-dict patterns elsewhere in this file.
  var __nodeRegistry = {};

  function recordMutation(mutation) {
    __mutations.push(mutation);
    // Notify content-positive MutationObservers (installed by shims.js)
    // immediately. Stub is no-op until shims.js installs the real one.
    if (typeof globalThis.__notifyMutationObservers === 'function') {
      try { globalThis.__notifyMutationObservers(mutation); } catch (e) {}
    }
  }

  globalThis.__getMutations = function() {
    var m = __mutations;
    __mutations = [];
    return m;
  };

  // Expose the id → node registry so the MutationObserver shim can
  // resolve mutation ids back to Node references for the MutationRecord
  // target/addedNodes/removedNodes fields.
  globalThis.__nodeById = function(id) {
    return __nodeRegistry[id] || null;
  };

  // --- Node Types ---
  var ELEMENT_NODE = 1;
  var TEXT_NODE = 3;
  var COMMENT_NODE = 8;
  var DOCUMENT_NODE = 9;
  var DOCUMENT_FRAGMENT_NODE = 11;

  // --- EventTarget ---
  function EventTarget() {
    this._listeners = {};
  }

  EventTarget.prototype.addEventListener = function(type, callback, options) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(callback);
  };

  EventTarget.prototype.removeEventListener = function(type, callback) {
    if (!this._listeners[type]) return;
    this._listeners[type] = this._listeners[type].filter(function(cb) { return cb !== callback; });
  };

  EventTarget.prototype.dispatchEvent = function(event) {
    event.target = this;
    event.currentTarget = this;
    // Invoke the IDL "event handler" property (el.onload, el.onclick, etc.)
    // before listeners — matches WHATWG ordering. Without this,
    // `s.onload = fn; appendChild(s)` paths (every dynamic-script loader,
    // every form.onsubmit, every button.onclick set inline) silently no-op.
    var prop = 'on' + event.type;
    if (typeof this[prop] === 'function') {
      try { this[prop].call(this, event); } catch (e) {}
    }
    var listeners = this._listeners[event.type] || [];
    for (var i = 0; i < listeners.length; i++) {
      listeners[i].call(this, event);
    }
    // Bubbling
    if (event.bubbles && this.parentNode && !event._stopped) {
      this.parentNode.dispatchEvent(event);
    }
    return !event.defaultPrevented;
  };

  // --- Event ---
  function Event(type, opts) {
    opts = opts || {};
    this.type = type;
    this.bubbles = opts.bubbles || false;
    this.cancelable = opts.cancelable || false;
    this.defaultPrevented = false;
    this._stopped = false;
    this.target = null;
    this.currentTarget = null;
  }

  Event.prototype.preventDefault = function() { this.defaultPrevented = true; };
  Event.prototype.stopPropagation = function() { this._stopped = true; };
  Event.prototype.stopImmediatePropagation = function() { this._stopped = true; };

  globalThis.Event = Event;
  globalThis.CustomEvent = function(type, opts) {
    Event.call(this, type, opts);
    this.detail = (opts && opts.detail) || null;
  };
  globalThis.CustomEvent.prototype = Object.create(Event.prototype);

  // --- ClassList ---
  function ClassList(element) {
    this._el = element;
  }

  ClassList.prototype.add = function() {
    var classes = (this._el.getAttribute('class') || '').split(/\s+/).filter(Boolean);
    for (var i = 0; i < arguments.length; i++) {
      if (classes.indexOf(arguments[i]) === -1) classes.push(arguments[i]);
    }
    this._el.setAttribute('class', classes.join(' '));
  };

  ClassList.prototype.remove = function() {
    var classes = (this._el.getAttribute('class') || '').split(/\s+/).filter(Boolean);
    for (var i = 0; i < arguments.length; i++) {
      var idx = classes.indexOf(arguments[i]);
      if (idx !== -1) classes.splice(idx, 1);
    }
    this._el.setAttribute('class', classes.join(' '));
  };

  ClassList.prototype.contains = function(cls) {
    return (this._el.getAttribute('class') || '').split(/\s+/).indexOf(cls) !== -1;
  };

  ClassList.prototype.toggle = function(cls, force) {
    if (force !== undefined) {
      if (force) this.add(cls); else this.remove(cls);
      return force;
    }
    if (this.contains(cls)) { this.remove(cls); return false; }
    this.add(cls); return true;
  };

  Object.defineProperty(ClassList.prototype, 'length', {
    get: function() { return (this._el.getAttribute('class') || '').split(/\s+/).filter(Boolean).length; }
  });

  // --- Style ---
  function CSSStyleDeclaration(element) {
    this._el = element;
    this._props = {};
  }

  CSSStyleDeclaration.prototype.setProperty = function(prop, value) {
    this._props[prop] = value;
    recordMutation({ type: 'setStyle', id: this._el._id, prop: prop, value: value });
  };

  CSSStyleDeclaration.prototype.getPropertyValue = function(prop) {
    return this._props[prop] || '';
  };

  CSSStyleDeclaration.prototype.removeProperty = function(prop) {
    var old = this._props[prop];
    delete this._props[prop];
    recordMutation({ type: 'setStyle', id: this._el._id, prop: prop, value: '' });
    return old || '';
  };

  Object.defineProperty(CSSStyleDeclaration.prototype, 'cssText', {
    get: function() {
      var parts = [];
      for (var k in this._props) parts.push(k + ': ' + this._props[k]);
      return parts.join('; ');
    },
    set: function(text) {
      this._props = {};
      if (!text) return;
      text.split(';').forEach(function(part) {
        var kv = part.split(':');
        if (kv.length === 2) {
          var prop = kv[0].trim();
          var val = kv[1].trim();
          if (prop) this._props[prop] = val;
        }
      }.bind(this));
      recordMutation({ type: 'setStyle', id: this._el._id, prop: '__cssText', value: text });
    }
  });

  // Proxy common style props (camelCase)
  var styleProps = ['display','color','backgroundColor','width','height','margin','padding',
    'border','fontSize','fontWeight','fontFamily','textAlign','position','top','left',
    'right','bottom','overflow','opacity','zIndex','visibility','cursor','textDecoration',
    'lineHeight','maxWidth','maxHeight','minWidth','minHeight','flex','flexDirection',
    'justifyContent','alignItems','gap','gridTemplateColumns','transform','transition',
    'boxShadow','borderRadius','outline','whiteSpace','wordBreak','float','clear'];

  function camelToDash(str) {
    return str.replace(/[A-Z]/g, function(m) { return '-' + m.toLowerCase(); });
  }

  styleProps.forEach(function(prop) {
    Object.defineProperty(CSSStyleDeclaration.prototype, prop, {
      get: function() { return this._props[camelToDash(prop)] || this._props[prop] || ''; },
      set: function(val) { this.setProperty(camelToDash(prop), val); }
    });
  });

  // --- Node ---
  function Node(nodeType) {
    EventTarget.call(this);
    this.nodeType = nodeType;
    this._id = __nextId++;
    this.parentNode = null;
    this.childNodes = [];
    this.ownerDocument = null;
    __nodeRegistry[this._id] = this;
  }

  Node.prototype = Object.create(EventTarget.prototype);
  Node.prototype.constructor = Node;

  // URL resolver. QuickJS has no native URL constructor, so we delegate to
  // Rust's url::Url::join via the __host_resolve_url host function (fully
  // spec-compliant: handles ../, ./, query-only, fragment-only, scheme-
  // relative, base normalization). The JS-side fallback below is kept only
  // as a safety net for environments that haven't installed the host fn —
  // it covers the simple cases (absolute, protocol-relative, root-relative,
  // simple path-relative) but does NOT handle ../, ./ or <base>. Reviewer
  // PR #6 medium: dynamic chunks commonly depend on correct base behavior.
  function __resolveURL(src, baseHref) {
    if (!src) return src;
    if (typeof __host_resolve_url === 'function') {
      try {
        var r = __host_resolve_url(src, baseHref || '');
        if (r) return r;
      } catch (e) { /* fall through to JS fallback */ }
    }
    // ---- JS-side fallback (limited) ----
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(src)) return src;          // absolute
    if (/^[a-z][a-z0-9+.-]*:/i.test(src)) return src;              // data:, blob:, etc.
    var m = baseHref.match(/^(https?):\/\/([^\/]+)(\/[^?#]*)?/i);
    if (!m) return src;
    var origin = m[1] + '://' + m[2];
    var path = m[3] || '/';
    if (src.indexOf('//') === 0) return m[1] + ':' + src;
    if (src.charAt(0) === '/') return origin + src;
    var dir = path.substring(0, path.lastIndexOf('/') + 1);
    return origin + dir + src;
  }

  // Dynamic script insertion handler.
  //
  // When page code does `s = createElement('script'); s.src = X; head.appendChild(s)`
  // (the way every tag manager and every dynamic chunk loader works), the
  // script must be fetched, evaluated, and have its `load` event fired so
  // the inserting code's onload callback runs. Without this, app boot stalls
  // anywhere a script awaits `s.onload`.
  //
  // This is the runtime equivalent of `main.rs`'s static script-fetch loop,
  // but for scripts inserted by JS *after* parse. It piggybacks on `fetch()`
  // (which routes through `__host_fetch_send` and therefore consults the
  // policy module — blocked URLs short-circuit with synthetic 204).
  //
  // Important pairing-with-static-loop invariant: scripts inserted during
  // `__seedDOM` use `buildChildren` (direct childNodes mutation), NOT
  // `appendChild`. Scripts in the parsed HTML are then extracted+evaled by
  // main.rs's static loop. Only TRULY dynamic insertions reach this path,
  // so there's no double-execution risk.
  //
  // For a blocked URL (synthetic 204 from policy), we still fire `load` —
  // not `error`. The page treats it as "the script tag manager loaded fine"
  // and proceeds with boot, but the cascade scripts the tag manager would
  // have injected never get a chance to run because its body never executed.
  // This is the "stub_success" pattern — block the actual code while keeping
  // the load callback chain intact.
  function __maybeHandleDynamicScript(child) {
    if (!child || child.tagName !== 'SCRIPT') return;
    if (child.__unbHandled) return;
    child.__unbHandled = true;

    var attrs = child._attributes || {};
    // Pages set s.src / s.type either via setAttribute (-> _attributes) or
    // direct property assignment (-> JS slot). Real browsers reflect IDL
    // properties to attributes; we don't, so check both. Direct property
    // wins when present (matches what the page believes it set).
    var type = ((child.type !== undefined ? child.type : attrs.type) || '').toString().toLowerCase();
    // Skip non-JS script types (JSON-LD, application/json, x-tmpl, etc.).
    // Empty type and "module" both count as JS.
    var isJs = type === '' || type === 'module' || type.indexOf('javascript') !== -1;
    if (!isJs) return;

    var src = child.src !== undefined ? child.src : attrs.src;
    if (src) {
      // QuickJS doesn't expose a `URL` constructor. Manual resolver covers
      // the cases we actually see: absolute, protocol-relative, root-relative,
      // path-relative. Anything weirder (e.g. data:, blob:) we pass through
      // unchanged and let the fetch/host layer handle.
      var baseHref = (typeof location !== 'undefined' && location.href) || '';
      var url = __resolveURL(src, baseHref);

      if (typeof fetch !== 'function') {
        // Pre-shim insertion (shouldn't happen in practice). Bail.
        return;
      }
      fetch(url).then(function(resp) {
        var status = resp.status;
        if (status >= 200 && status < 300 && status !== 204) {
          return resp.text().then(function(code) {
            // Try to evaluate. If eval throws (genuine syntax/runtime error,
            // OR ESM `import` syntax — type="module" is not actually
            // module-loaded; we evaluate it as classic script and surface
            // the failure), dispatch `error` so the page's onerror handler
            // sees it. This was previously swallowed and `load` fired
            // unconditionally, hiding real bugs and silently breaking ESM-
            // only chunks. (PR #6 review HIGH.)
            var evalOk = true;
            var evalErr = null;
            if (code) {
              try { (0, eval)(code); }
              catch (e) { evalOk = false; evalErr = e; }
            }
            if (evalOk) {
              child.dispatchEvent(new Event('load'));
            } else {
              // Surface eval failure to the page (matches browser behavior
              // for parse errors in fetched scripts) and to NDJSON via the
              // window error handler if any. Do not swallow.
              try {
                if (typeof globalThis.__unb_dyn_script_error === 'function') {
                  globalThis.__unb_dyn_script_error(url, String(evalErr && (evalErr.message || evalErr)));
                }
              } catch (_e) {}
              child.dispatchEvent(new Event('error'));
            }
          });
        } else if (status === 204) {
          // Synthetic 204 from the policy hook (or genuine 204 from server).
          // Fire load so the page's onload chain proceeds — this is the
          // "stub_success" pattern. Block the actual code execution while
          // keeping the load callback chain intact, so app boot doesn't
          // stall on a tag-manager-ready callback. The "no eval = success"
          // here is the policy decision, not a missing fetch.
          child.dispatchEvent(new Event('load'));
        } else {
          child.dispatchEvent(new Event('error'));
        }
      }).catch(function() {
        try { child.dispatchEvent(new Event('error')); } catch (e) {}
      });
    } else {
      // Inline script — eval immediately, then fire load asynchronously
      // (matches browser behavior: load events are not synchronous w.r.t.
      // the appendChild call site).
      var code = child.textContent || '';
      if (code) {
        try { (0, eval)(code); } catch (e) {}
      }
      Promise.resolve().then(function() {
        try { child.dispatchEvent(new Event('load')); } catch (e) {}
      });
    }
  }

  Node.prototype.appendChild = function(child) {
    if (child.nodeType === DOCUMENT_FRAGMENT_NODE) {
      var kids = child.childNodes.slice();
      for (var i = 0; i < kids.length; i++) this.appendChild(kids[i]);
      return child;
    }
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.childNodes.push(child);
    recordMutation({ type: 'appendChild', parentId: this._id, childId: child._id, childDef: serializeNode(child) });
    __maybeHandleDynamicScript(child);
    return child;
  };

  Node.prototype.removeChild = function(child) {
    var idx = this.childNodes.indexOf(child);
    if (idx === -1) throw new Error('Node not found');
    this.childNodes.splice(idx, 1);
    child.parentNode = null;
    recordMutation({ type: 'removeChild', parentId: this._id, childId: child._id });
    return child;
  };

  Node.prototype.insertBefore = function(newChild, refChild) {
    if (!refChild) return this.appendChild(newChild);
    if (newChild.nodeType === DOCUMENT_FRAGMENT_NODE) {
      var kids = newChild.childNodes.slice();
      for (var i = 0; i < kids.length; i++) this.insertBefore(kids[i], refChild);
      return newChild;
    }
    if (newChild.parentNode) newChild.parentNode.removeChild(newChild);
    var idx = this.childNodes.indexOf(refChild);
    if (idx === -1) throw new Error('Ref node not found');
    newChild.parentNode = this;
    this.childNodes.splice(idx, 0, newChild);
    recordMutation({ type: 'insertBefore', parentId: this._id, childId: newChild._id, refId: refChild._id, childDef: serializeNode(newChild) });
    __maybeHandleDynamicScript(newChild);
    return newChild;
  };

  Node.prototype.replaceChild = function(newChild, oldChild) {
    this.insertBefore(newChild, oldChild);
    this.removeChild(oldChild);
    return oldChild;
  };

  Node.prototype.cloneNode = function(deep) {
    if (this.nodeType === TEXT_NODE) {
      return document.createTextNode(this.textContent);
    }
    var clone = document.createElement(this.tagName);
    for (var k in this._attributes) clone.setAttribute(k, this._attributes[k]);
    if (deep) {
      for (var i = 0; i < this.childNodes.length; i++) {
        clone.appendChild(this.childNodes[i].cloneNode(true));
      }
    }
    return clone;
  };

  Node.prototype.contains = function(node) {
    if (node === this) return true;
    for (var i = 0; i < this.childNodes.length; i++) {
      if (this.childNodes[i].contains(node)) return true;
    }
    return false;
  };

  Node.prototype.hasChildNodes = function() { return this.childNodes.length > 0; };

  Object.defineProperty(Node.prototype, 'firstChild', {
    get: function() { return this.childNodes[0] || null; }
  });
  Object.defineProperty(Node.prototype, 'lastChild', {
    get: function() { return this.childNodes[this.childNodes.length - 1] || null; }
  });
  Object.defineProperty(Node.prototype, 'nextSibling', {
    get: function() {
      if (!this.parentNode) return null;
      var idx = this.parentNode.childNodes.indexOf(this);
      return this.parentNode.childNodes[idx + 1] || null;
    }
  });
  Object.defineProperty(Node.prototype, 'previousSibling', {
    get: function() {
      if (!this.parentNode) return null;
      var idx = this.parentNode.childNodes.indexOf(this);
      return this.parentNode.childNodes[idx - 1] || null;
    }
  });

  // --- TextNode ---
  function TextNode(text) {
    Node.call(this, TEXT_NODE);
    this.textContent = text || '';
    this.nodeName = '#text';
    this.data = this.textContent;
    this.nodeValue = this.textContent;
  }

  TextNode.prototype = Object.create(Node.prototype);
  TextNode.prototype.constructor = TextNode;

  // --- Element ---
  function Element(tagName) {
    Node.call(this, ELEMENT_NODE);
    this.tagName = tagName.toUpperCase();
    this.nodeName = this.tagName;
    this._attributes = {};
    this.style = new CSSStyleDeclaration(this);
    this.classList = new ClassList(this);
    this.dataset = {};
  }

  Element.prototype = Object.create(Node.prototype);
  Element.prototype.constructor = Element;

  Element.prototype.getAttribute = function(name) {
    return this._attributes[name] !== undefined ? this._attributes[name] : null;
  };

  Element.prototype.setAttribute = function(name, value) {
    this._attributes[name] = String(value);
    if (name === 'id') this.id = value;
    recordMutation({ type: 'setAttribute', id: this._id, name: name, value: String(value) });
  };

  Element.prototype.removeAttribute = function(name) {
    delete this._attributes[name];
    recordMutation({ type: 'removeAttribute', id: this._id, name: name });
  };

  Element.prototype.hasAttribute = function(name) {
    return name in this._attributes;
  };

  Element.prototype.matches = function(selector) {
    return matchesSelector(this, parseSelector(selector));
  };

  Element.prototype.closest = function(selector) {
    var parsed = parseSelector(selector);
    var node = this;
    while (node) {
      if (node.nodeType === ELEMENT_NODE && matchesSelector(node, parsed)) return node;
      node = node.parentNode;
    }
    return null;
  };

  Element.prototype.querySelector = function(selector) {
    return querySelector(this, selector);
  };

  Element.prototype.querySelectorAll = function(selector) {
    return querySelectorAll(this, selector);
  };

  Element.prototype.getElementsByTagName = function(tag) {
    tag = tag.toUpperCase();
    var results = [];
    function walk(node) {
      for (var i = 0; i < node.childNodes.length; i++) {
        var child = node.childNodes[i];
        if (child.nodeType === ELEMENT_NODE) {
          if (tag === '*' || child.tagName === tag) results.push(child);
          walk(child);
        }
      }
    }
    walk(this);
    return results;
  };

  Element.prototype.getElementsByClassName = function(cls) {
    var results = [];
    function walk(node) {
      for (var i = 0; i < node.childNodes.length; i++) {
        var child = node.childNodes[i];
        if (child.nodeType === ELEMENT_NODE) {
          if (child.classList.contains(cls)) results.push(child);
          walk(child);
        }
      }
    }
    walk(this);
    return results;
  };

  Element.prototype.getBoundingClientRect = function() {
    return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 };
  };

  Element.prototype.focus = function() {};
  Element.prototype.blur = function() {};
  Element.prototype.click = function() {
    this.dispatchEvent(new Event('click', { bubbles: true }));
  };

  // ChildNode mixin — remove(), before(), after(), replaceWith().
  // Standard DOM (Element + CharacterData inherit from ChildNode). Without
  // these, idiomatic JS like `el.remove()` throws TypeError, which agents
  // call frequently when iterating with eval.
  Element.prototype.remove = function() {
    if (this.parentNode) this.parentNode.removeChild(this);
  };
  TextNode.prototype.remove = function() {
    if (this.parentNode) this.parentNode.removeChild(this);
  };
  Element.prototype.replaceWith = function() {
    var parent = this.parentNode;
    if (!parent) return;
    var ref = this;
    for (var i = 0; i < arguments.length; i++) {
      var node = arguments[i];
      if (typeof node === 'string') node = document.createTextNode(node);
      parent.insertBefore(node, ref);
    }
    parent.removeChild(this);
  };

  Object.defineProperty(Element.prototype, 'id', {
    get: function() { return this._attributes.id || ''; },
    set: function(v) { this._attributes.id = v; }
  });

  Object.defineProperty(Element.prototype, 'className', {
    get: function() { return this._attributes['class'] || ''; },
    set: function(v) { this.setAttribute('class', v); }
  });

  // Boolean HTML attributes that mirror to a same-named JS property. Real
  // browsers maintain both: the attribute reflects parsed-from-HTML state,
  // the property is the live runtime state. For our purposes that means
  // "if the page set `<input checked>` in source, `el.checked === true`,
  // and toggling el.checked in JS later sticks." We store the override in
  // a per-element bag; absent that, fall back to the attribute presence.
  ['checked', 'disabled', 'selected', 'readOnly'].forEach(function(prop) {
    var attrName = prop === 'readOnly' ? 'readonly' : prop;
    Object.defineProperty(Element.prototype, prop, {
      get: function() {
        if (this._boolProps && attrName in this._boolProps) return this._boolProps[attrName];
        return this.hasAttribute(attrName);
      },
      set: function(v) {
        if (!this._boolProps) this._boolProps = {};
        this._boolProps[attrName] = !!v;
      },
      configurable: true,
    });
  });

  Object.defineProperty(Element.prototype, 'textContent', {
    get: function() {
      var text = '';
      for (var i = 0; i < this.childNodes.length; i++) {
        var c = this.childNodes[i];
        if (c.nodeType === TEXT_NODE) text += c.textContent;
        else if (c.nodeType === ELEMENT_NODE) text += c.textContent;
      }
      return text;
    },
    set: function(value) {
      this.childNodes = [];
      if (value) {
        var tn = new TextNode(value);
        tn.parentNode = this;
        this.childNodes.push(tn);
      }
      recordMutation({ type: 'setTextContent', id: this._id, value: value || '' });
    }
  });

  Object.defineProperty(Element.prototype, 'innerHTML', {
    get: function() {
      return serializeChildren(this);
    },
    set: function(html) {
      this.childNodes = [];
      recordMutation({ type: 'setInnerHTML', id: this._id, html: html });
      // Parse HTML and build nodes using host-provided parser
      if (html && typeof __parseHTMLFragment === 'function') {
        var tree = __parseHTMLFragment(html);
        buildChildren(this, tree);
      }
    }
  });

  Object.defineProperty(Element.prototype, 'outerHTML', {
    get: function() {
      var attrs = '';
      for (var k in this._attributes) attrs += ' ' + k + '="' + this._attributes[k] + '"';
      var tag = this.tagName.toLowerCase();
      return '<' + tag + attrs + '>' + this.innerHTML + '</' + tag + '>';
    }
  });

  Object.defineProperty(Element.prototype, 'children', {
    get: function() {
      return this.childNodes.filter(function(c) { return c.nodeType === ELEMENT_NODE; });
    }
  });

  Object.defineProperty(Element.prototype, 'firstElementChild', {
    get: function() {
      for (var i = 0; i < this.childNodes.length; i++) {
        if (this.childNodes[i].nodeType === ELEMENT_NODE) return this.childNodes[i];
      }
      return null;
    }
  });

  Object.defineProperty(Element.prototype, 'nextElementSibling', {
    get: function() {
      if (!this.parentNode) return null;
      var found = false;
      for (var i = 0; i < this.parentNode.childNodes.length; i++) {
        var c = this.parentNode.childNodes[i];
        if (found && c.nodeType === ELEMENT_NODE) return c;
        if (c === this) found = true;
      }
      return null;
    }
  });

  Object.defineProperty(Element.prototype, 'previousElementSibling', {
    get: function() {
      if (!this.parentNode) return null;
      var prev = null;
      for (var i = 0; i < this.parentNode.childNodes.length; i++) {
        var c = this.parentNode.childNodes[i];
        if (c === this) return prev;
        if (c.nodeType === ELEMENT_NODE) prev = c;
      }
      return null;
    }
  });

  // --- Serialization helpers ---
  function serializeNode(node) {
    if (node.nodeType === TEXT_NODE) {
      return { type: 'text', id: node._id, content: node.textContent };
    }
    var attrs = {};
    for (var k in node._attributes) attrs[k] = node._attributes[k];
    return {
      type: 'element',
      id: node._id,
      tag: node.tagName.toLowerCase(),
      attrs: attrs,
      children: node.childNodes.map(serializeNode)
    };
  }

  function serializeChildren(el) {
    var html = '';
    for (var i = 0; i < el.childNodes.length; i++) {
      var c = el.childNodes[i];
      if (c.nodeType === TEXT_NODE) html += c.textContent;
      else html += c.outerHTML;
    }
    return html;
  }

  function buildChildren(parent, tree) {
    if (!tree || !tree.children) return;
    for (var i = 0; i < tree.children.length; i++) {
      var def = tree.children[i];
      if (def.type === 'text') {
        var tn = new TextNode(def.content);
        tn.parentNode = parent;
        parent.childNodes.push(tn);
      } else if (def.type === 'element') {
        var el = new Element(def.tag);
        if (def.attrs) {
          for (var k in def.attrs) {
            el._attributes[k] = def.attrs[k];
            if (k === 'id') el.id = def.attrs[k];
          }
        }
        el.parentNode = parent;
        parent.childNodes.push(el);
        if (def.children) buildChildren(el, def);
      }
    }
  }

  // --- CSS Selector Engine ---
  function parseSelector(sel) {
    // Split on commas for multiple selectors
    return sel.split(',').map(function(s) { return s.trim(); });
  }

  // Tokenize a single selector, inserting spaces around combinators (>, +, ~)
  // while respecting bracket/paren depth so attribute selectors like [a~=b]
  // and pseudo-class args like :nth-child(2n+1) aren't broken up.
  function tokenize(sel) {
    var out = '';
    var bracket = 0, paren = 0;
    for (var i = 0; i < sel.length; i++) {
      var c = sel[i];
      if (c === '[') bracket++;
      else if (c === ']') bracket--;
      else if (c === '(') paren++;
      else if (c === ')') paren--;
      if (bracket === 0 && paren === 0 && (c === '>' || c === '+' || c === '~')) {
        out += ' ' + c + ' ';
      } else {
        out += c;
      }
    }
    return out.split(/\s+/).filter(Boolean);
  }

  function matchesPseudo(el, name, arg) {
    var parent = el.parentNode;
    if (!parent) return false;

    // 1-based position of el among parent's element children, optionally restricted to same tag.
    function indexInParent(sameTagOnly) {
      var idx = 0, pos = 0;
      for (var i = 0; i < parent.childNodes.length; i++) {
        var c = parent.childNodes[i];
        if (c.nodeType !== ELEMENT_NODE) continue;
        if (sameTagOnly && c.tagName !== el.tagName) continue;
        idx++;
        if (c === el) pos = idx;
      }
      return { pos: pos, total: idx };
    }

    // CSS An+B parsing. Accepts: integer, 'odd', 'even', 'n', '-n+N', '2n+1',
    // '3n-2', etc. Returns {a, b} or null. p must satisfy p = a*k + b for some
    // integer k >= 0 with p >= 1.
    function parseAnB(spec) {
      if (spec == null) return null;
      var s = String(spec).replace(/\s+/g, '').toLowerCase();
      if (s === 'odd') return { a: 2, b: 1 };
      if (s === 'even') return { a: 2, b: 0 };
      if (s.indexOf('n') === -1) {
        var n = parseInt(s, 10);
        return isNaN(n) ? null : { a: 0, b: n };
      }
      var m = s.match(/^([+-]?\d*)n([+-]\d+)?$/);
      if (!m) return null;
      var a = m[1] === '' || m[1] === '+' ? 1 : (m[1] === '-' ? -1 : parseInt(m[1], 10));
      var b = m[2] ? parseInt(m[2], 10) : 0;
      return { a: a, b: b };
    }

    function matchN(spec, p) {
      var ab = parseAnB(spec);
      if (!ab) return false;
      if (ab.a === 0) return p === ab.b;
      var k = (p - ab.b) / ab.a;
      return k >= 0 && Number.isInteger(k);
    }

    var info;
    switch (name) {
      case 'first-child':   info = indexInParent(false); return info.pos === 1;
      case 'last-child':    info = indexInParent(false); return info.pos === info.total && info.pos > 0;
      case 'first-of-type': info = indexInParent(true);  return info.pos === 1;
      case 'last-of-type':  info = indexInParent(true);  return info.pos === info.total && info.pos > 0;
      case 'nth-child':     return matchN(arg, indexInParent(false).pos);
      case 'nth-of-type':   return matchN(arg, indexInParent(true).pos);
      case 'only-child':    info = indexInParent(false); return info.total === 1;
      case 'only-of-type':  info = indexInParent(true);  return info.total === 1;
      case 'not':           return !matchesSelector(el, [arg]);
      case 'has': {
        // True if any descendant matches the inner selector. Cheap recursive
        // walk; bounded by the tree size we already paid to build.
        var stack = [].concat(el.childNodes || []);
        while (stack.length) {
          var c = stack.shift();
          if (c.nodeType === ELEMENT_NODE) {
            if (matchesSelector(c, [arg])) return true;
            for (var i = 0; i < (c.childNodes || []).length; i++) stack.push(c.childNodes[i]);
          }
        }
        return false;
      }
      default: return false;
    }
  }

  function matchesSingle(el, part) {
    // Peel pseudos off the right (with paren/bracket-depth tracking so
    // `:not(div:hover)`-style nested colons don't fool the splitter). The
    // remainder is a bare tag#id.class[attr=val] simple selector — handled
    // by the regex below. Supports chained pseudos like `div:has(p):not(.x)`.
    var pseudos = [];
    var s = part;
    while (true) {
      var depth = 0, lastColon = -1;
      for (var i = 0; i < s.length; i++) {
        var c = s[i];
        if (c === '(' || c === '[') depth++;
        else if (c === ')' || c === ']') depth--;
        else if (c === ':' && depth === 0) lastColon = i;
      }
      if (lastColon === -1) break;
      var psPart = s.slice(lastColon + 1);
      var pm = psPart.match(/^([a-z-]+)(?:\((.+)\))?$/i);
      if (!pm) break;
      pseudos.unshift({ name: pm[1], arg: pm[2] });
      s = s.slice(0, lastColon);
    }

    // Simple selector: tag#id.class[attr=val] or `*` (universal)
    var re = /^(\*|[a-zA-Z0-9_-]*)?(?:#([a-zA-Z0-9_-]+))?(?:\.([a-zA-Z0-9_. -]+))?(?:\[([a-zA-Z0-9_-]+)(?:([~|^$*]?)=["']?([^"'\]]*?)["']?)?\])?$/;
    var m = s.match(re);
    if (!m) return false;

    var tag = m[1], id = m[2], classes = m[3];
    var attrName = m[4], attrOp = m[5], attrVal = m[6];

    // `*` matches any tag; non-empty tag must match exactly.
    if (tag && tag !== '*' && el.tagName !== tag.toUpperCase()) return false;
    if (id && el.id !== id) return false;
    if (classes) {
      var clsList = classes.split('.');
      for (var ci = 0; ci < clsList.length; ci++) {
        if (clsList[ci] && !el.classList.contains(clsList[ci])) return false;
      }
    }
    if (attrName) {
      var val = el.getAttribute(attrName);
      if (val === null) return false;
      if (attrVal !== undefined) {
        if (attrOp === '' || attrOp === undefined) { if (val !== attrVal) return false; }
        else if (attrOp === '^') { if (!val.startsWith(attrVal)) return false; }
        else if (attrOp === '$') { if (!val.endsWith(attrVal)) return false; }
        else if (attrOp === '*') { if (val.indexOf(attrVal) === -1) return false; }
        else if (attrOp === '~') { if (val.split(/\s+/).indexOf(attrVal) === -1) return false; }
      }
    }
    for (var pi = 0; pi < pseudos.length; pi++) {
      if (!matchesPseudo(el, pseudos[pi].name, pseudos[pi].arg)) return false;
    }
    return true;
  }

  // Recursive right-to-left matching, supporting descendant ' ', child '>',
  // adjacent sibling '+', and general sibling '~' combinators.
  function matchChain(el, parts, idx) {
    if (idx < 0) return true;
    var combinator, simpleIdx;
    var p = parts[idx];
    if (p === '>' || p === '+' || p === '~') {
      combinator = p;
      simpleIdx = idx - 1;
    } else {
      combinator = ' ';
      simpleIdx = idx;
    }
    if (simpleIdx < 0) return false;
    var sel = parts[simpleIdx];

    if (combinator === ' ') {
      var node = el.parentNode;
      while (node && node.nodeType === ELEMENT_NODE) {
        if (matchesSingle(node, sel) && matchChain(node, parts, simpleIdx - 1)) return true;
        node = node.parentNode;
      }
      return false;
    }
    if (combinator === '>') {
      var par = el.parentNode;
      if (!par || par.nodeType !== ELEMENT_NODE) return false;
      if (!matchesSingle(par, sel)) return false;
      return matchChain(par, parts, simpleIdx - 1);
    }
    if (combinator === '+') {
      var prev = el.previousElementSibling;
      if (!prev || !matchesSingle(prev, sel)) return false;
      return matchChain(prev, parts, simpleIdx - 1);
    }
    if (combinator === '~') {
      var sib = el.previousElementSibling;
      while (sib) {
        if (matchesSingle(sib, sel) && matchChain(sib, parts, simpleIdx - 1)) return true;
        sib = sib.previousElementSibling;
      }
      return false;
    }
    return false;
  }

  function matchesSelector(el, selectors) {
    for (var s = 0; s < selectors.length; s++) {
      var parts = tokenize(selectors[s]);
      if (parts.length === 0) continue;
      if (!matchesSingle(el, parts[parts.length - 1])) continue;
      if (matchChain(el, parts, parts.length - 2)) return true;
    }
    return false;
  }

  function querySelector(root, selector) {
    var parsed = parseSelector(selector);
    var result = null;
    function walk(node) {
      for (var i = 0; i < node.childNodes.length; i++) {
        var child = node.childNodes[i];
        if (result) return;
        if (child.nodeType === ELEMENT_NODE) {
          if (matchesSelector(child, parsed)) { result = child; return; }
          walk(child);
        }
      }
    }
    walk(root);
    return result;
  }

  function querySelectorAll(root, selector) {
    var parsed = parseSelector(selector);
    var results = [];
    function walk(node) {
      for (var i = 0; i < node.childNodes.length; i++) {
        var child = node.childNodes[i];
        if (child.nodeType === ELEMENT_NODE) {
          if (matchesSelector(child, parsed)) results.push(child);
          walk(child);
        }
      }
    }
    walk(root);
    return results;
  }

  // --- Document ---
  var htmlEl = new Element('html');
  var headEl = new Element('head');
  var bodyEl = new Element('body');
  htmlEl.childNodes = [headEl, bodyEl];
  headEl.parentNode = htmlEl;
  bodyEl.parentNode = htmlEl;

  var document = {
    nodeType: DOCUMENT_NODE,
    documentElement: htmlEl,
    head: headEl,
    body: bodyEl,
    title: '',
    cookie: '',
    readyState: 'loading',

    createElement: function(tag) { return new Element(tag); },
    createTextNode: function(text) { return new TextNode(text); },
    createComment: function(text) { var n = new Node(COMMENT_NODE); n.textContent = text; return n; },
    createDocumentFragment: function() { var n = new Node(DOCUMENT_FRAGMENT_NODE); n.childNodes = []; return n; },

    getElementById: function(id) {
      function walk(node) {
        if (node.nodeType === ELEMENT_NODE && node.id === id) return node;
        for (var i = 0; i < (node.childNodes || []).length; i++) {
          var r = walk(node.childNodes[i]);
          if (r) return r;
        }
        return null;
      }
      return walk(htmlEl);
    },

    querySelector: function(sel) { return querySelector(htmlEl, sel); },
    querySelectorAll: function(sel) { return querySelectorAll(htmlEl, sel); },
    getElementsByTagName: function(tag) { return htmlEl.getElementsByTagName(tag); },
    getElementsByClassName: function(cls) { return htmlEl.getElementsByClassName(cls); },

    createEvent: function(type) { return new Event(type); },

    addEventListener: function(type, fn) {
      if (!document._listeners) document._listeners = {};
      if (!document._listeners[type]) document._listeners[type] = [];
      document._listeners[type].push(fn);
    },
    removeEventListener: function(type, fn) {
      if (!document._listeners || !document._listeners[type]) return;
      document._listeners[type] = document._listeners[type].filter(function(f) { return f !== fn; });
    },
    dispatchEvent: function(event) {
      var listeners = (document._listeners || {})[event.type] || [];
      for (var i = 0; i < listeners.length; i++) listeners[i](event);
    }
  };

  // --- Seed DOM from parsed JSON tree ---
  globalThis.__seedDOM = function(tree) {
    // Clear existing
    bodyEl.childNodes = [];
    headEl.childNodes = [];
    // Clear node registry — surviving nodes (htmlEl, headEl, bodyEl,
    // documentEl) reinsert themselves below via Element / Node
    // constructors during buildChildren. Stale nodes from a previous
    // navigate would otherwise accumulate.
    __nodeRegistry = {};
    __nodeRegistry[htmlEl._id] = htmlEl;
    __nodeRegistry[headEl._id] = headEl;
    __nodeRegistry[bodyEl._id] = bodyEl;

    if (tree.tag === 'html') {
      for (var i = 0; i < (tree.children || []).length; i++) {
        var child = tree.children[i];
        if (child.tag === 'head' && child.children) {
          buildChildren(headEl, child);
          if (child.attrs) for (var k in child.attrs) headEl._attributes[k] = child.attrs[k];
        } else if (child.tag === 'body' && child.children) {
          buildChildren(bodyEl, child);
          if (child.attrs) for (var k2 in child.attrs) bodyEl._attributes[k2] = child.attrs[k2];
        }
      }
    } else {
      buildChildren(bodyEl, { children: [tree] });
    }

    // Set title
    var titleEl = querySelector(headEl, 'title');
    if (titleEl) document.title = titleEl.textContent;
  };

  // --- Serialize full DOM for bridge ---
  globalThis.__serializeDOM = function() {
    return serializeNode(htmlEl);
  };

  // Expose globals
  globalThis.document = document;
  globalThis.Document = { prototype: document };
  globalThis.Element = Element;
  globalThis.Node = Node;
  globalThis.HTMLElement = Element;
  globalThis.Text = TextNode;
  globalThis.DocumentFragment = Node;

})();
