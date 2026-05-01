// Interactivity helpers — click, type, form data extraction.
// Element refs are 'e:NN' strings; __byRef walks the DOM and resolves them.

(function() {
  globalThis.__byRef = function(ref) {
    var m = String(ref || '').match(/^e:(\d+)$/);
    if (!m) return null;
    var id = parseInt(m[1], 10);
    function walk(node) {
      if (!node) return null;
      if (node.nodeType === 1 && node._id === id) return node;
      var kids = node.childNodes || [];
      for (var i = 0; i < kids.length; i++) {
        var r = walk(kids[i]);
        if (r) return r;
      }
      return null;
    }
    return walk(document.documentElement);
  };

  globalThis.__click = function(ref) {
    var el = __byRef(ref);
    if (!el) return { ok: false, error: 'no element for ' + ref };
    var ev = new Event('click', { bubbles: true, cancelable: true });
    el.dispatchEvent(ev);
    // Default action: follow <a href>, OR toggle checkbox/radio state. Caller
    // (Rust) decides whether to navigate. Only suppressed if the page called
    // ev.preventDefault() inside its click handler.
    var follow = null;
    var checked = null;
    if (!ev.defaultPrevented) {
      if (el.tagName === 'A' && el.getAttribute('href')) {
        follow = el.getAttribute('href');
      } else if (el.tagName === 'INPUT') {
        var t = (el.getAttribute('type') || '').toLowerCase();
        if (t === 'checkbox') {
          el.checked = !el.checked;
          checked = el.checked;
          el.dispatchEvent(new Event('change', { bubbles: true }));
        } else if (t === 'radio') {
          var name = el.getAttribute('name');
          if (name) {
            // Uncheck siblings within the closest <form> (or document if no form).
            var scope = el;
            while (scope && scope.tagName !== 'FORM') scope = scope.parentNode;
            scope = scope || document;
            var siblings = scope.querySelectorAll('input[type=radio][name="' + name + '"]');
            for (var i = 0; i < siblings.length; i++) {
              if (siblings[i] !== el) siblings[i].checked = false;
            }
          }
          el.checked = true;
          checked = true;
          el.dispatchEvent(new Event('change', { bubbles: true }));
        }
      }
    }
    return {
      ok: true,
      ref: ref,
      tag: el.tagName.toLowerCase(),
      follow: follow,
      checked: checked,
    };
  };

  globalThis.__type = function(ref, text) {
    var el = __byRef(ref);
    if (!el) return { ok: false, error: 'no element for ' + ref };
    var s = String(text == null ? '' : text);
    el.setAttribute('value', s);
    el.value = s;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true, ref: ref, tag: el.tagName.toLowerCase(), value: s };
  };

  globalThis.__formData = function(ref) {
    var el = __byRef(ref);
    if (!el) return { ok: false, error: 'no element for ' + ref };
    if (el.tagName !== 'FORM') return { ok: false, error: ref + ' is not a <form> (got ' + el.tagName + ')' };
    var fields = [];
    var inputs = el.querySelectorAll('input, textarea, select');
    for (var i = 0; i < inputs.length; i++) {
      var inp = inputs[i];
      var name = inp.getAttribute('name');
      if (!name) continue;
      var type = (inp.getAttribute('type') || 'text').toLowerCase();
      if (type === 'submit' || type === 'button' || type === 'reset' || type === 'image') continue;
      // Checkbox/radio: only emit when checked. Browsers serialize the value
      // attr ('on' default for checkboxes), and skip the field entirely when
      // unchecked. We mirror that.
      if (type === 'checkbox' || type === 'radio') {
        if (!inp.checked) continue;
        var cval = (inp.value !== undefined && inp.value !== null && inp.value !== '')
          ? inp.value
          : (inp.getAttribute('value') || 'on');
        fields.push([name, String(cval)]);
        continue;
      }
      var val = (inp.value !== undefined && inp.value !== null) ? inp.value : (inp.getAttribute('value') || '');
      fields.push([name, String(val)]);
    }
    var enctype = (el.getAttribute('enctype') || 'application/x-www-form-urlencoded').toLowerCase();
    return {
      ok: true,
      action: el.getAttribute('action') || '',
      method: (el.getAttribute('method') || 'get').toLowerCase(),
      enctype: enctype,
      fields: fields,
    };
  };
})();
