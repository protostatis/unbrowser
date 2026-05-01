// BlockMap — DOM walk → semantic block summary + ASCII outline.
// Replaces visual rendering for LLM page orientation. Cheap O(N) walks.

(function() {
  function divider(n) {
    return new Array((n || 40) + 1).join('─');
  }

  function shortIdent(el) {
    var tag = el.tagName.toLowerCase();
    var id = el.getAttribute('id');
    var cls = el.getAttribute('class');
    var s = tag;
    if (id) s += '#' + id;
    if (cls) {
      var first = cls.split(/\s+/).filter(Boolean).slice(0, 2).join('.');
      if (first) s += '.' + first;
    }
    return s;
  }

  function clean(s) {
    return (s || '').replace(/\s+/g, ' ').trim();
  }

  function summarize(el) {
    var counts = {
      links: el.getElementsByTagName('a').length,
      buttons: el.getElementsByTagName('button').length,
      inputs: el.querySelectorAll('input, textarea, select').length,
      headings: el.querySelectorAll('h1, h2, h3, h4, h5, h6').length,
      lists: el.getElementsByTagName('ul').length + el.getElementsByTagName('ol').length,
      tables: el.getElementsByTagName('table').length,
      images: el.getElementsByTagName('img').length,
    };
    var parts = [];
    if (counts.headings) parts.push(counts.headings + ' headings');
    if (counts.links) parts.push(counts.links + ' links');
    if (counts.buttons) parts.push(counts.buttons + ' buttons');
    if (counts.inputs) parts.push(counts.inputs + ' inputs');
    if (counts.tables) parts.push(counts.tables + ' tables');
    if (counts.lists) parts.push(counts.lists + ' lists');
    if (counts.images) parts.push(counts.images + ' images');
    var firstHeading = '';
    var fh = el.querySelectorAll('h1, h2, h3')[0];
    if (fh) firstHeading = clean(fh.textContent).slice(0, 60);
    return {
      role: el.getAttribute('role') || el.tagName.toLowerCase(),
      ref: 'e:' + el._id,
      ident: shortIdent(el),
      counts: counts,
      summary: (firstHeading ? '"' + firstHeading + '" — ' : '') + (parts.join(', ') || 'empty'),
    };
  }

  globalThis.__blockmap = function() {
    var body = document.body;
    if (!body) {
      return {
        title: document.title || '',
        structure: [],
        headings: [],
        interactives: { links: 0, buttons: 0, inputs: [], forms: [] },
        ascii: '(no body)'
      };
    }

    // Headings — keep up to 20.
    var headings = [];
    var hs = body.querySelectorAll('h1, h2, h3, h4, h5, h6');
    for (var i = 0; i < hs.length && i < 20; i++) {
      headings.push({
        level: parseInt(hs[i].tagName[1], 10),
        text: clean(hs[i].textContent).slice(0, 80),
        ref: 'e:' + hs[i]._id,
      });
    }

    // Interactives
    var links = body.getElementsByTagName('a');
    var buttons = body.getElementsByTagName('button');
    var inputsRaw = body.querySelectorAll('input, textarea, select');
    var inputs = [];
    for (var j = 0; j < inputsRaw.length; j++) {
      var inp = inputsRaw[j];
      inputs.push({
        ref: 'e:' + inp._id,
        tag: inp.tagName.toLowerCase(),
        type: inp.getAttribute('type') || 'text',
        name: inp.getAttribute('name') || null,
        placeholder: inp.getAttribute('placeholder') || null,
        value: inp.getAttribute('value') || null,
      });
    }
    var formEls = body.getElementsByTagName('form');
    var forms = [];
    for (var k = 0; k < formEls.length; k++) {
      var f = formEls[k];
      forms.push({
        ref: 'e:' + f._id,
        action: f.getAttribute('action') || '',
        method: (f.getAttribute('method') || 'get').toLowerCase(),
        fields: f.querySelectorAll('input, textarea, select').length,
      });
    }

    // Structure: HTML5 landmarks first; fall back to significant top-level children.
    var structure = [];
    var landmarks = body.querySelectorAll('header, nav, main, aside, footer, article, section');
    for (var m = 0; m < landmarks.length; m++) {
      structure.push(summarize(landmarks[m]));
    }
    if (structure.length === 0) {
      var children = body.children;
      for (var c = 0; c < children.length; c++) {
        var ch = children[c];
        if (ch.getElementsByTagName('*').length >= 5) {
          structure.push(summarize(ch));
        }
      }
    }

    // ASCII outline
    var ascii = [];
    var bar = '  ' + divider(64);
    ascii.push('  ' + (document.title || '(untitled)'));
    ascii.push(bar);
    if (structure.length === 0) {
      ascii.push('  (no landmarks or significant top-level blocks)');
    } else {
      for (var s = 0; s < structure.length; s++) {
        var b = structure[s];
        var role = (b.role.toUpperCase() + '          ').slice(0, 9);
        ascii.push('  ' + role + ' [' + b.ref + '] ' + b.ident + ' — ' + b.summary);
      }
    }
    ascii.push(bar);
    if (headings.length) {
      ascii.push('  HEADINGS (' + headings.length + ')');
      for (var h = 0; h < headings.length && h < 8; h++) {
        var indent = new Array(headings[h].level + 1).join(' ');
        ascii.push('    ' + indent + 'h' + headings[h].level + ' ' + headings[h].text);
      }
    }
    ascii.push('  INTERACTIVES: ' + links.length + ' links · ' + buttons.length + ' buttons · ' + inputs.length + ' inputs · ' + forms.length + ' forms');

    // Data-density signal: distinguishes "fully SSR'd" pages from "SSR shell
    // with JS-populated cells" (e.g. CNBC tables, financial dashboards). Three
    // signals, OR'd: empty <td>s, empty <li>s, or empty <table> shells (the
    // worst case — page has table tags but rows/cells get JS-injected, so no
    // <td> exists at all in the static HTML).
    function densityOf(els, threshold) {
      if (!els || els.length === 0) return null;
      var filled = 0;
      var minLen = threshold || 2;
      for (var di = 0; di < els.length; di++) {
        var t = (els[di].textContent || '').replace(/\s+/g, ' ').trim();
        if (t.length >= minLen) filled++;
      }
      var ratio = filled / els.length;
      return {
        total: els.length,
        filled: filled,
        ratio: Math.round(ratio * 1000) / 1000,
      };
    }
    var tdDensity = densityOf(body.getElementsByTagName('td'), 2);
    var liDensity = densityOf(body.getElementsByTagName('li'), 2);
    // For tables, "empty" = under 5 chars of textContent (the table tag itself
    // and whitespace). Threshold higher because tables have wrapper noise.
    var tableDensity = densityOf(body.getElementsByTagName('table'), 5);

    function suspicious(d, minTotal) {
      return d != null && d.total >= (minTotal || 20) && d.ratio < 0.4;
    }
    var likelyJsFilled =
      suspicious(tdDensity, 20) ||
      suspicious(liDensity, 30) ||
      suspicious(tableDensity, 3);   // even a few empty tables is a strong signal

    // JSON script tags often carry the data the JS rendering would fill in
    // (Next.js __NEXT_DATA__, ld+json, custom application/json blocks).
    // Hint to the agent: try `eval` to read these directly instead of waiting
    // for client-side rendering.
    var jsonScripts = document.querySelectorAll(
      'script[type="application/json"], script[type="application/ld+json"]'
    ).length;

    // Fold into the ASCII summary.
    var hasDensity = tdDensity || liDensity || tableDensity;
    if (hasDensity) {
      var densityLine = '  DATA DENSITY:';
      if (tableDensity) densityLine += ' tables=' + tableDensity.filled + '/' + tableDensity.total;
      if (tdDensity)    densityLine += ' td=' + tdDensity.filled + '/' + tdDensity.total + ' (' + Math.round(tdDensity.ratio * 100) + '%)';
      if (liDensity)    densityLine += ' li=' + liDensity.filled + '/' + liDensity.total + ' (' + Math.round(liDensity.ratio * 100) + '%)';
      if (likelyJsFilled) densityLine += '  ⚠ likely JS-filled (cells empty)';
      ascii.push(densityLine);
    }
    if (jsonScripts > 0) {
      ascii.push('  JSON SCRIPTS: ' + jsonScripts + ' (data may be embedded — try eval on document.querySelector(\'script[type=\"application/json\"]\').textContent)');
    }

    return {
      title: document.title || '',
      structure: structure,
      headings: headings,
      interactives: {
        links: links.length,
        buttons: buttons.length,
        inputs: inputs,
        forms: forms,
      },
      density: {
        tables: tableDensity,
        td: tdDensity,
        li: liDensity,
        json_scripts: jsonScripts,
        likely_js_filled: likelyJsFilled,
      },
      ascii: ascii.join('\n'),
    };
  };
})();
