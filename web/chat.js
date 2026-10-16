(function () {
  'use strict';
  var $ = function (id) { return document.getElementById(id); };
  var root = document.documentElement;
  var STORE = 'hui-chat';

  // ------------------------------------------------------------------ theme --
  try { var saved = localStorage.getItem('hui-theme'); if (saved) root.setAttribute('data-theme', saved); } catch (e) { /* storage blocked */ }
  $('theme').addEventListener('click', function () {
    var dark = root.getAttribute('data-theme') ? root.getAttribute('data-theme') === 'dark'
      : window.matchMedia('(prefers-color-scheme: dark)').matches;
    var next = dark ? 'light' : 'dark';
    root.setAttribute('data-theme', next);
    try { localStorage.setItem('hui-theme', next); } catch (e) { /* ignore */ }
  });

  // ------------------------------------------------------------------ state --
  // msgs: { role: 'user'|'assistant', content, reasoning?, files?: [{name,text}], images?: [dataUrl], nimg?, stat? }
  var msgs = [], pending = [], busy = false, ctrl = null, model = null, stick = true;
  var settings = { system: '', think: '', temp: '', max: '' };
  var chat = $('chat'), thread = $('thread'), input = $('input'), form = $('form');

  function load() {
    try {
      var s = JSON.parse(localStorage.getItem(STORE) || 'null');
      if (s && Array.isArray(s.msgs)) msgs = s.msgs;
      if (s && s.settings) for (var k in settings) if (typeof s.settings[k] === 'string') settings[k] = s.settings[k];
    } catch (e) { /* nothing saved, or storage blocked */ }
  }
  function save() {
    // images are heavy: only their count is kept across reloads
    var slim = msgs.map(function (m) {
      var c = {}; for (var k in m) c[k] = m[k];
      if (c.images && c.images.length) { c.nimg = c.images.length; delete c.images; }
      return c;
    });
    try { localStorage.setItem(STORE, JSON.stringify({ msgs: slim, settings: settings })); } catch (e) { /* full or blocked */ }
  }

  // --------------------------------------------------------------- markdown --
  // Small, safe renderer: builds DOM nodes (never innerHTML) and only links http(s) and mailto.
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }
  function copyText(s, btn) {
    var done = function () { if (!btn) return; var t = btn.textContent; btn.textContent = 'Copied'; setTimeout(function () { btn.textContent = t; }, 1200); };
    if (navigator.clipboard && window.isSecureContext) { navigator.clipboard.writeText(s).then(done, function () { /* denied */ }); return; }
    var ta = el('textarea'); ta.value = s; ta.style.position = 'fixed'; ta.style.opacity = '0';
    document.body.appendChild(ta); ta.select();
    try { document.execCommand('copy'); done(); } catch (e) { /* ignore */ }
    document.body.removeChild(ta);
  }
  function text(parent, s) {
    s.split('\n').forEach(function (part, i) {
      if (i) parent.appendChild(el('br'));
      if (part) parent.appendChild(document.createTextNode(part));
    });
  }
  var INLINE = /(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)|\*\*([^\s*][\s\S]*?)\*\*|~~([\s\S]+?)~~|\[([^\]\n]+)\]\((https?:\/\/[^\s)]+|mailto:[^\s)]+)\)|(https?:\/\/[^\s<>"')\]]+)|\*([^\s*][^*\n]*?)\*|\b_([^_\n]+)_\b/g;
  function inline(parent, s) {
    var re = new RegExp(INLINE.source, 'g'), at = 0, m;
    while ((m = re.exec(s))) {
      if (m.index > at) text(parent, s.slice(at, m.index));
      var n;
      if (m[2] != null) n = el('code', null, m[2].trim());
      else if (m[3] != null) { n = el('strong'); inline(n, m[3]); }
      else if (m[4] != null) { n = el('del'); inline(n, m[4]); }
      else if (m[5] != null) { n = el('a'); n.href = m[6]; n.target = '_blank'; n.rel = 'noopener noreferrer nofollow'; inline(n, m[5]); }
      else if (m[7] != null) { n = el('a', null, m[7]); n.href = m[7]; n.target = '_blank'; n.rel = 'noopener noreferrer nofollow'; }
      else { n = el('em'); inline(n, m[8] != null ? m[8] : m[9]); }
      parent.appendChild(n);
      at = re.lastIndex;
    }
    if (at < s.length) text(parent, s.slice(at));
  }
  function codeBlock(lang, code) {
    var pre = el('pre'), head = el('div', 'code-head'), btn = el('button', null, 'Copy');
    btn.type = 'button';
    btn.addEventListener('click', function () { copyText(code, btn); });
    head.appendChild(el('span', null, lang || 'code')); head.appendChild(btn);
    pre.appendChild(head); pre.appendChild(el('code', null, code));
    return pre;
  }
  var LI = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;
  var FENCE = /^\s{0,3}(`{3,}|~{3,})\s*([\w+#.-]*)/;
  var HEAD = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
  var HR = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
  var TSEP = /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/;
  function indentOf(l) { return /^\s*/.exec(l)[0].replace(/\t/g, '    ').length; }
  function startsBlock(l) { return FENCE.test(l) || HEAD.test(l) || HR.test(l) || /^\s{0,3}>/.test(l) || LI.test(l); }
  function cells(l) { return l.trim().replace(/^\|/, '').replace(/\|$/, '').split(/(?<!\\)\|/).map(function (c) { return c.trim().replace(/\\\|/g, '|'); }); }

  function list(lines, i) {
    var first = LI.exec(lines[i]), base = indentOf(lines[i]), ordered = /\d/.test(first[2].charAt(0));
    var ul = el(ordered ? 'ol' : 'ul');
    if (ordered && parseInt(first[2], 10) !== 1) ul.start = parseInt(first[2], 10);
    while (i < lines.length) {
      var m = LI.exec(lines[i]);
      if (!m || indentOf(lines[i]) > base + 1 || indentOf(lines[i]) < base - 1) break;
      var li = el('li'), body = m[3];
      i++;
      var kids = [];
      while (i < lines.length) {
        var l = lines[i];
        if (/^\s*$/.test(l)) {
          var j = i; while (j < lines.length && /^\s*$/.test(lines[j])) j++;
          if (j < lines.length && (LI.test(lines[j]) || indentOf(lines[j]) > base + 1)) { i = j; continue; }
          break;
        }
        var ind = indentOf(l);
        if (LI.test(l)) {
          if (ind > base + 1) { var r = list(lines, i); kids.push(r[0]); i = r[1]; continue; }
          break;
        }
        if (ind > base || !startsBlock(l)) { body += '\n' + l.trim(); i++; continue; }
        break;
      }
      inline(li, body);
      kids.forEach(function (k) { li.appendChild(k); });
      ul.appendChild(li);
    }
    return [ul, i];
  }

  function md(src) {
    var frag = document.createDocumentFragment(), lines = src.replace(/\r\n?/g, '\n').split('\n'), i = 0, m;
    while (i < lines.length) {
      var l = lines[i];
      if (/^\s*$/.test(l)) { i++; continue; }
      if ((m = FENCE.exec(l))) {
        var fence = m[1], code = [];
        i++;
        while (i < lines.length && !(new RegExp('^\\s{0,3}' + fence.charAt(0) + '{' + fence.length + ',}\\s*$')).test(lines[i])) code.push(lines[i++]);
        i++; // closing fence (absent while the answer is still streaming)
        frag.appendChild(codeBlock(m[2], code.join('\n')));
      } else if ((m = HEAD.exec(l))) {
        var h = el('h' + m[1].length); inline(h, m[2]); frag.appendChild(h); i++;
      } else if (HR.test(l)) {
        frag.appendChild(el('hr')); i++;
      } else if (/^\s{0,3}>/.test(l)) {
        var q = [];
        while (i < lines.length && /^\s{0,3}>/.test(lines[i])) q.push(lines[i++].replace(/^\s{0,3}>\s?/, ''));
        var bq = el('blockquote'); bq.appendChild(md(q.join('\n'))); frag.appendChild(bq);
      } else if (l.indexOf('|') >= 0 && i + 1 < lines.length && TSEP.test(lines[i + 1]) && lines[i + 1].indexOf('-') >= 0) {
        var wrap = el('div', 'tbl'), t = el('table'), thead = el('thead'), tr = el('tr');
        cells(l).forEach(function (c) { var th = el('th'); inline(th, c); tr.appendChild(th); });
        thead.appendChild(tr); t.appendChild(thead);
        var tb = el('tbody');
        i += 2;
        while (i < lines.length && lines[i].indexOf('|') >= 0 && !/^\s*$/.test(lines[i])) {
          var row = el('tr');
          cells(lines[i++]).forEach(function (c) { var td = el('td'); inline(td, c); row.appendChild(td); });
          tb.appendChild(row);
        }
        t.appendChild(tb); wrap.appendChild(t); frag.appendChild(wrap);
      } else if (LI.test(l)) {
        var r = list(lines, i); frag.appendChild(r[0]); i = r[1];
      } else {
        var para = [l]; i++;
        while (i < lines.length && !/^\s*$/.test(lines[i]) && !startsBlock(lines[i])) para.push(lines[i++]);
        var p = el('p'); inline(p, para.join('\n')); frag.appendChild(p);
      }
    }
    return frag;
  }

  // --------------------------------------------------------------- messages --
  function nearBottom() { return chat.scrollHeight - chat.scrollTop - chat.clientHeight < 90; }
  chat.addEventListener('scroll', function () { stick = nearBottom(); });
  function toBottom() { if (stick) chat.scrollTop = chat.scrollHeight; }
  function started() { chat.classList.toggle('started', msgs.length > 0 || thread.childNodes.length > 0); }

  function fmtStat(s) {
    if (!s) return '';
    var bits = [];
    if (s.tokens != null) bits.push(s.tokens + ' tokens');
    if (s.tps != null) bits.push(s.tps.toFixed(1) + ' tok/s');
    if (s.secs != null) bits.push(s.secs.toFixed(1) + ' s');
    if (s.note) bits.push(s.note);
    return bits.join(' · ');
  }

  function userEl(m) {
    var box = el('div', 'msg user');
    if ((m.images && m.images.length) || m.nimg) {
      var pics = el('div', 'pics');
      (m.images || []).forEach(function (u) { var im = el('img'); im.src = u; im.alt = 'attached image'; pics.appendChild(im); });
      if (!(m.images && m.images.length)) pics.appendChild(el('span', 'filechip', m.nimg + (m.nimg > 1 ? ' images' : ' image') + ' (not kept)'));
      box.appendChild(pics);
    }
    if (m.files && m.files.length) {
      var fs = el('div', 'pics');
      m.files.forEach(function (f) { fs.appendChild(el('span', 'filechip', '📄 ' + f.name)); });
      box.appendChild(fs);
    }
    if (m.content) box.appendChild(el('div', 'bubble', m.content));
    return box;
  }

  // The live parts of an assistant message: thinking block, answer, footer.
  function assistantEl(m) {
    var box = el('div', 'msg assistant'), ui = { box: box, m: m };
    ui.think = el('details', 'think'); ui.think.hidden = true;
    ui.thinkSum = el('summary', null, 'Thinking');
    ui.thinkBody = el('div', 'think-body');
    ui.think.appendChild(ui.thinkSum); ui.think.appendChild(ui.thinkBody);
    ui.body = el('div', 'bubble md');
    ui.foot = el('div', 'msg-foot');
    box.appendChild(ui.think); box.appendChild(ui.body); box.appendChild(ui.foot);
    paint(ui, false);
    return ui;
  }
  function paint(ui, live) {
    var m = ui.m;
    if (m.reasoning) {
      ui.think.hidden = false;
      ui.thinkBody.textContent = m.reasoning;
      ui.think.classList.toggle('live', live && !m.content);
      ui.thinkSum.textContent = live && !m.content ? 'Thinking' : 'Thought' + (m.thinkSecs ? ' for ' + m.thinkSecs.toFixed(0) + ' s' : '');
      if (live && !m.content) { ui.think.open = true; ui.thinkBody.scrollTop = ui.thinkBody.scrollHeight; }
    }
    ui.body.textContent = '';
    if (m.content) ui.body.appendChild(md(m.content));
    else if (live && !m.reasoning) ui.body.appendChild(el('span', 'waiting', 'Waiting for the model…'));
    ui.body.classList.toggle('caret', live && !!m.content);
    toBottom();
  }
  function footer(ui, last) {
    var m = ui.m;
    ui.foot.textContent = '';
    if (!m.content && !m.reasoning) return;
    if (m.stat) ui.foot.appendChild(el('span', 'stat', fmtStat(m.stat)));
    var c = el('button', null, 'Copy'); c.type = 'button';
    c.addEventListener('click', function () { copyText(m.content, c); });
    ui.foot.appendChild(c);
    if (last) {
      var r = el('button', null, 'Regenerate'); r.type = 'button';
      r.addEventListener('click', function () { if (!busy) regenerate(); });
      ui.foot.appendChild(r);
    }
  }
  function notice(msg) {
    var box = el('div', 'msg error');
    box.appendChild(el('div', 'bubble', msg));
    thread.appendChild(box); started(); toBottom();
  }

  function renderAll() {
    thread.textContent = '';
    var lastA = -1;
    msgs.forEach(function (m, i) { if (m.role === 'assistant') lastA = i; });
    msgs.forEach(function (m, i) {
      if (m.role === 'user') thread.appendChild(userEl(m));
      else { var ui = assistantEl(m); footer(ui, i === lastA && i === msgs.length - 1); thread.appendChild(ui.box); }
    });
    started();
    stick = true; toBottom();
  }

  // ---------------------------------------------------------------- request --
  function userContent(m) {
    var t = (m.files || []).map(function (f) { return '`' + f.name + '`:\n```\n' + f.text + '\n```'; });
    if (m.content) t.push(m.content);
    var txt = t.join('\n\n');
    if (!m.images || !m.images.length) return txt;
    var parts = [{ type: 'text', text: txt || 'Describe this image.' }];
    m.images.forEach(function (u) { parts.push({ type: 'image_url', image_url: { url: u } }); });
    return parts;
  }
  function payload(upto) {
    var out = [];
    if (settings.system.trim()) out.push({ role: 'system', content: settings.system.trim() });
    msgs.slice(0, upto).forEach(function (m) {
      if (m.role === 'user') out.push({ role: 'user', content: userContent(m) });
      else if (m.content) out.push({ role: 'assistant', content: m.content });
    });
    var b = { messages: out, stream: true, stream_options: { include_usage: true } };
    if (model) b.model = model;
    if (settings.temp !== '' && isFinite(+settings.temp)) b.temperature = +settings.temp;
    if (settings.max !== '' && +settings.max > 0) b.max_tokens = Math.floor(+settings.max);
    if (settings.think === 'off') b.enable_thinking = false;
    else if (settings.think) b.reasoning_effort = settings.think;
    return b;
  }

  function setBusy(b) {
    busy = b;
    form.classList.toggle('busy', b);
    $('send').title = b ? 'Stop' : 'Send';
    $('send').setAttribute('aria-label', b ? 'Stop' : 'Send');
    sync();
  }

  function run() {
    var m = { role: 'assistant', content: '', reasoning: '' };
    msgs.push(m);
    var ui = assistantEl(m);
    thread.appendChild(ui.box); started();
    stick = true;
    paint(ui, true);
    setBusy(true);
    ctrl = new AbortController();
    var t0 = performance.now(), tFirst = null, tThinkEnd = null, usage = null, finish = null, raf = 0;
    var schedule = function () { if (!raf) raf = requestAnimationFrame(function () { raf = 0; paint(ui, true); }); };

    function onEvent(d) {
      if (d.error) throw new Error(d.error.message || 'the model returned an error');
      if (d.usage) usage = d.usage;
      var c = d.choices && d.choices[0];
      if (!c) return;
      if (c.finish_reason) finish = c.finish_reason;
      var dl = c.delta || {};
      var r = dl.reasoning_content || dl.reasoning;
      if (r) { if (tFirst == null) tFirst = performance.now(); m.reasoning += r; }
      if (dl.content) {
        if (tFirst == null) tFirst = performance.now();
        if (tThinkEnd == null && m.reasoning) { tThinkEnd = performance.now(); m.thinkSecs = (tThinkEnd - t0) / 1000; }
        m.content += dl.content;
      }
      if (r || dl.content) schedule();
    }

    fetch('api/chat', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, signal: ctrl.signal,
      body: JSON.stringify(payload(msgs.length - 1))
    }).then(function (res) {
      if (!res.ok) {
        return res.text().then(function (t) {
          var msg = t;
          try { var j = JSON.parse(t); msg = (j.error && j.error.message) || j.detail || t; } catch (e) { /* plain text */ }
          var err = new Error('HTTP ' + res.status + (msg ? ': ' + String(msg).slice(0, 400) : ''));
          err.status = res.status;
          throw err;
        });
      }
      var reader = res.body.getReader(), dec = new TextDecoder(), buf = '';
      return (function pump() {
        return reader.read().then(function (r) {
          if (r.done) return;
          buf += dec.decode(r.value, { stream: true });
          var parts = buf.split(/\r?\n\r?\n/);
          buf = parts.pop();
          parts.forEach(function (ev) {
            ev.split(/\r?\n/).forEach(function (line) {
              if (line.indexOf('data:') !== 0) return;
              var data = line.slice(5).trim();
              if (!data || data === '[DONE]') return;
              var j; try { j = JSON.parse(data); } catch (e) { return; }
              onEvent(j);
            });
          });
          return pump();
        });
      })();
    }).catch(function (e) {
      if (e.name === 'AbortError') { m.stat = m.stat || {}; m.stat.note = 'stopped'; return; }
      var partial = m.content || m.reasoning;
      if (!partial) msgs.pop();
      if (!partial) thread.removeChild(ui.box);
      if (e.status === 401 && !partial) { askPassword(run); return; }
      notice('Could not get an answer. ' + (e.message || e));
    }).then(function () {
      if (raf) cancelAnimationFrame(raf);
      ctrl = null;
      var t1 = performance.now();
      if (m.content || m.reasoning) {
        var toks = usage && usage.completion_tokens;
        m.stat = m.stat || {};
        if (toks != null) { m.stat.tokens = toks; if (tFirst != null && t1 - tFirst > 50) m.stat.tps = toks / ((t1 - tFirst) / 1000); }
        m.stat.secs = (t1 - t0) / 1000;
        if (finish === 'length') m.stat.note = 'reached the token limit';
        paint(ui, false); footer(ui, true);
      }
      setBusy(false);
      save();
      input.focus();
    });
  }

  function regenerate() {
    var last = msgs[msgs.length - 1];
    if (!last || last.role !== 'assistant') return;
    msgs.pop();
    renderAll();
    run();
  }

  function stop() { if (ctrl) ctrl.abort(); }

  function send() {
    var t = input.value.trim();
    if (!t && !pending.length) return;
    var m = { role: 'user', content: t };
    var imgs = pending.filter(function (p) { return p.kind === 'img'; }).map(function (p) { return p.url; });
    var files = pending.filter(function (p) { return p.kind === 'txt'; }).map(function (p) { return { name: p.name, text: p.text }; });
    if (imgs.length) m.images = imgs;
    if (files.length) m.files = files;
    // the last answer no longer ends the conversation: drop its Regenerate button
    Array.prototype.forEach.call(thread.querySelectorAll('.msg-foot button'), function (b) {
      if (b.textContent === 'Regenerate') b.remove();
    });
    msgs.push(m);
    thread.appendChild(userEl(m));
    input.value = ''; pending = []; drawAtts(); grow();
    run();
  }

  // ------------------------------------------------------------ attachments --
  var MAX_IMAGES = 8, MAX_TEXT = 1 << 20;
  function drawAtts() {
    var box = $('atts');
    box.textContent = '';
    box.hidden = !pending.length;
    pending.forEach(function (p, i) {
      var a = el('div', 'att');
      if (p.kind === 'img') { var im = el('img'); im.src = p.url; im.alt = ''; a.appendChild(im); }
      a.appendChild(el('span', 'name', (p.kind === 'txt' ? '📄 ' : '') + p.name));
      var x = el('button', null, '×'); x.type = 'button'; x.setAttribute('aria-label', 'Remove ' + p.name);
      x.addEventListener('click', function () { pending.splice(i, 1); drawAtts(); sync(); });
      a.appendChild(x); box.appendChild(a);
    });
    sync();
  }
  function shrink(file) {
    // Halogen caps image size: send at most 1920 px on the long side, as JPEG (PNG with transparency stays PNG)
    return new Promise(function (ok, ko) {
      var url = URL.createObjectURL(file), im = new Image();
      im.onload = function () {
        var k = Math.min(1, 1920 / Math.max(im.naturalWidth, im.naturalHeight));
        var c = el('canvas'); c.width = Math.max(1, Math.round(im.naturalWidth * k)); c.height = Math.max(1, Math.round(im.naturalHeight * k));
        var g = c.getContext('2d');
        if (file.type !== 'image/png') { g.fillStyle = '#fff'; g.fillRect(0, 0, c.width, c.height); }
        g.drawImage(im, 0, 0, c.width, c.height);
        URL.revokeObjectURL(url);
        ok(c.toDataURL(file.type === 'image/png' ? 'image/png' : 'image/jpeg', 0.9));
      };
      im.onerror = function () { URL.revokeObjectURL(url); ko(new Error('not an image the browser can read')); };
      im.src = url;
    });
  }
  function addFiles(list) {
    Array.prototype.forEach.call(list, function (f) {
      if (/^image\//.test(f.type)) {
        if (pending.filter(function (p) { return p.kind === 'img'; }).length >= MAX_IMAGES) { notice('At most ' + MAX_IMAGES + ' images per message.'); return; }
        shrink(f).then(function (u) { pending.push({ kind: 'img', name: f.name || 'image', url: u }); drawAtts(); },
          function (e) { notice(f.name + ': ' + e.message); });
      } else if (f.size <= MAX_TEXT) {
        var r = new FileReader();
        r.onload = function () {
          var s = String(r.result);
          if (s.indexOf('\u0000') >= 0) { notice(f.name + ': binary files are not supported.'); return; }
          pending.push({ kind: 'txt', name: f.name, text: s }); drawAtts();
        };
        r.onerror = function () { notice('Could not read ' + f.name + '.'); };
        r.readAsText(f);
      } else notice(f.name + ': text files up to 1 MB only.');
    });
  }
  $('attach').addEventListener('click', function () { $('file').click(); });
  $('file').addEventListener('change', function () { addFiles(this.files); this.value = ''; });
  input.addEventListener('paste', function (e) {
    var fs = e.clipboardData && e.clipboardData.files;
    if (fs && fs.length) { e.preventDefault(); addFiles(fs); }
  });
  document.addEventListener('dragover', function (e) { if (e.dataTransfer && e.dataTransfer.types.indexOf('Files') >= 0) e.preventDefault(); });
  document.addEventListener('drop', function (e) {
    if (e.dataTransfer && e.dataTransfer.files.length) { e.preventDefault(); addFiles(e.dataTransfer.files); }
  });

  // --------------------------------------------------------------- composer --
  function grow() { input.style.height = 'auto'; input.style.height = Math.min(input.scrollHeight, window.innerHeight * 0.4) + 'px'; }
  function sync() { $('send').disabled = !busy && !input.value.trim() && !pending.length; }
  input.addEventListener('input', function () { grow(); sync(); });
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); if (!busy) send(); }
    if (e.key === 'Escape' && busy) stop();
  });
  form.addEventListener('submit', function (e) { e.preventDefault(); if (busy) stop(); else send(); });

  $('new').addEventListener('click', function () {
    stop(); msgs = []; pending = []; drawAtts();
    thread.textContent = ''; started(); save(); input.focus();
  });
  $('export').addEventListener('click', function () {
    if (!msgs.length) return;
    var md = msgs.map(function (m) {
      var body = m.content;
      if (m.role === 'user' && m.files) body = m.files.map(function (f) { return '[' + f.name + ']'; }).join(' ') + '\n\n' + body;
      return '**' + (m.role === 'user' ? 'You' : 'Model') + '**\n\n' + body;
    }).join('\n\n---\n\n');
    var d = new Date(), p = function (n) { return String(n).padStart(2, '0'); };
    var a = el('a'); a.href = URL.createObjectURL(new Blob([md + '\n'], { type: 'text/markdown' }));
    a.download = 'chat-' + d.getFullYear() + p(d.getMonth() + 1) + p(d.getDate()) + '-' + p(d.getHours()) + p(d.getMinutes()) + '.md';
    document.body.appendChild(a); a.click(); document.body.removeChild(a);
    setTimeout(function () { URL.revokeObjectURL(a.href); }, 2000);
  });

  // --------------------------------------------------------------- settings --
  var fields = { system: 's-system', think: 's-think', temp: 's-temp', max: 's-max' };
  Object.keys(fields).forEach(function (k) {
    var f = $(fields[k]);
    f.addEventListener('input', function () { settings[k] = f.value; save(); });
  });
  $('cog').addEventListener('click', function () {
    var open = $('settings').hidden;
    $('settings').hidden = !open;
    this.setAttribute('aria-expanded', String(open));
  });

  // --------------------------------------------------------------- password --
  // Only when the server was started with HUI_CHAT_TOKEN: a correct password opens a session cookie.
  function askPassword(then) {
    var lock = $('lock'), pw = $('lock-pw'), err = $('lock-err');
    lock.hidden = false; err.hidden = true; pw.value = ''; pw.focus();
    function go() {
      fetch('api/chat-login', { method: 'POST', headers: { 'Content-Type': 'text/plain' }, body: pw.value }).then(function (r) {
        if (!r.ok) { err.textContent = r.status === 401 ? 'Wrong password.' : 'Could not sign in (HTTP ' + r.status + ').'; err.hidden = false; pw.select(); return; }
        lock.hidden = true; pw.value = '';
        if (then) then(); else input.focus();
      }, function () { err.textContent = 'Server unreachable.'; err.hidden = false; });
    }
    $('lock-go').onclick = go;
    pw.onkeydown = function (e) { if (e.key === 'Enter') go(); };
  }
  fetch('api/chat-login', { cache: 'no-store' }).then(function (r) { return r.json(); }).then(function (s) {
    if (s.required && !s.ok) askPassword(null);
  }).catch(function () { /* the first message will say what is wrong */ });

  // ------------------------------------------------------- server status pill --
  var PHASE = { idle: 'Idle', reading: 'Reading', generating: 'Generating', down: 'Down', unreachable: 'Unreachable' };
  function poll() {
    fetch('api/state', { cache: 'no-store' }).then(function (r) { return r.json(); }).then(function (s) {
      var ph = s.status.phase;
      $('pill').setAttribute('data-p', s.status.queued ? 'queued' : ph);
      $('pill-txt').textContent = s.status.queued ? 'Queued' : PHASE[ph] || ph;
      if (s.model && s.model.name) {
        model = s.model.name;
        $('hero-sub').textContent = model + ' runs on this server. Nothing leaves it.';
      }
    }).catch(function () {
      $('pill').setAttribute('data-p', 'unreachable'); $('pill-txt').textContent = 'Unreachable';
    });
  }

  // ------------------------------------------------------------------- start --
  load();
  Object.keys(fields).forEach(function (k) { $(fields[k]).value = settings[k]; });
  renderAll(); drawAtts(); grow(); sync();
  poll(); setInterval(poll, 3000);
  input.focus();
})();
