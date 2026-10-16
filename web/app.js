(function () {
  'use strict';
  var $ = function (id) { return document.getElementById(id); };
  var loc = undefined; // the viewer's locale
  var nf = function (d) { return new Intl.NumberFormat(loc, { minimumFractionDigits: d, maximumFractionDigits: d }); };
  var f0 = nf(0), f1 = nf(1);
  var showAll = false, busy = false, failures = 0;

  // ------------------------------------------------------------------ theme --
  var root = document.documentElement;
  try { var saved = localStorage.getItem('hui-theme'); if (saved) root.setAttribute('data-theme', saved); } catch (e) { /* storage blocked */ }
  $('theme').addEventListener('click', function () {
    var dark = root.getAttribute('data-theme') ? root.getAttribute('data-theme') === 'dark'
      : window.matchMedia('(prefers-color-scheme: dark)').matches;
    var next = dark ? 'light' : 'dark';
    root.setAttribute('data-theme', next);
    try { localStorage.setItem('hui-theme', next); } catch (e) { /* ignore */ }
    if (last) render(last);
  });
  $('show-all').addEventListener('click', function () { showAll = !showAll; tick(); });

  // ---------------------------------------------------------------- helpers --
  function num(v, d) { return v == null || !isFinite(v) ? '–' : (d ? f1 : f0).format(v); }
  function unit(v, u, d) { return v == null || !isFinite(v) ? '–' : num(v, d) + '<small>' + u + '</small>'; }
  function gib(b) { return b == null ? null : b / 1073741824; }
  function kfmt(n) { return n == null ? '–' : n >= 1e6 ? f1.format(n / 1e6) + ' M' : n >= 1e4 ? f0.format(n / 1e3) + 'k' : f0.format(n); }
  function pct(a, b) { return a != null && b ? 100 * a / b : null; }
  function text(id, s) { $(id).textContent = s; }
  function html(id, s) { $(id).innerHTML = s; }
  function clock(t) { return new Date(t * 1000).toLocaleTimeString(loc, { hour: '2-digit', minute: '2-digit', second: '2-digit' }); }
  function dur(s) { return s == null ? '–' : s < 60 ? f1.format(s) + ' s' : Math.floor(s / 60) + ' min ' + f0.format(s % 60) + ' s'; }
  function cssVar(n) { return getComputedStyle(root).getPropertyValue(n).trim(); }
  function setW(id, p) { $(id).style.width = Math.max(0, Math.min(100, p || 0)) + '%'; }

  // Numbers glide to their new value instead of jumping: `draw(v)` is called on every frame with the value in between.
  var still = window.matchMedia('(prefers-reduced-motion: reduce)').matches, glides = {};
  function glide(id, target, draw) {
    var g = glides[id] || (glides[id] = { v: null, raf: 0 });
    cancelAnimationFrame(g.raf);
    if (target == null || !isFinite(target) || g.v == null || still) { g.v = target == null || !isFinite(target) ? null : target; draw(g.v); return; }
    var from = g.v, t0 = performance.now(), len = 450;
    (function step(now) {
      var k = Math.min(1, (now - t0) / len);
      g.v = from + (target - from) * (1 - Math.pow(1 - k, 3));
      draw(g.v);
      if (k < 1) g.raf = requestAnimationFrame(step);
    })(t0);
  }

  function spark(id, values, max) {
    var c = $(id), dpr = window.devicePixelRatio || 1, w = c.clientWidth, h = c.clientHeight;
    if (!w || !h) return;
    if (c.width !== Math.round(w * dpr)) { c.width = Math.round(w * dpr); c.height = Math.round(h * dpr); }
    var g = c.getContext('2d');
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, h);
    var v = values.filter(function (x) { return x != null && isFinite(x); });
    if (v.length < 2) return;
    var top = max || Math.max.apply(null, v) * 1.1 || 1;
    var stroke = c.classList.contains('warm') ? cssVar('--amber') : c.classList.contains('blue') ? cssVar('--blue') : cssVar('--green');
    var fill = c.classList.contains('warm') ? cssVar('--amber-fill') : c.classList.contains('blue') ? cssVar('--blue-fill') : cssVar('--green-fill');
    var step = w / (v.length - 1);
    g.beginPath();
    v.forEach(function (x, i) { var y = h - 2 - (Math.min(x, top) / top) * (h - 4); if (i) g.lineTo(i * step, y); else g.moveTo(0, y); });
    g.strokeStyle = stroke; g.lineWidth = 1.6; g.lineJoin = 'round'; g.stroke();
    g.lineTo(w, h); g.lineTo(0, h); g.closePath(); g.fillStyle = fill; g.fill();
  }

  // ----------------------------------------------------------------- render --
  var PHASE = { idle: 'Idle', reading: 'Reading', generating: 'Generating', down: 'Down', unreachable: 'Unreachable' };
  var last = null, prevRows = null;

  function render(s) {
    var st = s.status, ph = st.phase, m = s.metrics, h = s.host, r0 = s.requests[0];
    document.title = s.title + ' · ' + (PHASE[ph] || ph);

    // header pill and chips
    $('pill').setAttribute('data-p', st.queued ? 'queued' : ph);
    text('pill-txt', st.queued ? 'Queued' : PHASE[ph] || ph);
    Array.prototype.forEach.call(document.querySelectorAll('.chip'), function (c) {
      var p = c.getAttribute('data-p');
      c.classList.toggle('on', p === ph || (p === 'queued' && st.queued) || (p === 'down' && ph === 'unreachable'));
    });

    // state line and bar
    var bar = $('state-bar').parentNode, txt;
    bar.classList.toggle('gen', ph === 'generating');
    if (ph === 'reading' && st.total) {
      // Halogen logs prefill progress every few seconds: the elapsed time keeps running in between
      var el = st.secs + Math.max(0, s.now - st.seen);
      txt = 'Reading the prompt · ' + f0.format(st.done) + ' / ' + f0.format(st.total) + ' tokens · ' + f1.format(el) + ' s';
      setW('state-bar', pct(st.done, st.total));
    } else if (ph === 'generating') {
      var lv = s.live, busyTxt = st.busy_for ? ' · busy for ' + dur(st.busy_for) : '';
      if (lv && lv.tokens) {
        // tokens counted live on the chat streams (Halogen itself only reports a request's tokens when it ends)
        glide('live', lv.tokens, function (v) {
          text('state-txt', 'Generating · ' + f0.format(v) + ' tokens' + (lv.tps ? ' · ' + f1.format(lv.tps) + ' tok/s' : '') + busyTxt + (st.queued ? ' · ' + f0.format(st.waiting) + ' waiting' : ''));
        });
        txt = null;
      } else txt = 'Generating' + busyTxt;
      setW('state-bar', 100);
    } else if (ph === 'idle') {
      txt = 'Waiting for a request'; setW('state-bar', 0);
    } else if (ph === 'down') {
      txt = 'The engine is down'; setW('state-bar', 0);
    } else {
      txt = 'Halogen does not answer'; setW('state-bar', 0);
    }
    if (st.queued && txt != null) txt += ' · ' + f0.format(st.waiting) + ' waiting';
    if (txt != null) text('state-txt', txt);
    if (!(ph === 'generating' && s.live && s.live.tokens)) glide('live', null, function () { });
    text('state-last', r0 ? 'last: ' + f0.format(r0.output) + ' tokens at ' + f1.format(r0.decode_tps) + ' tok/s' : '');

    // banner: what is missing
    var problems = [];
    if (!s.metrics_ok) problems.push('Metrics: ' + s.metrics_error);
    if (!s.log_ok && s.log_error) problems.push('Log: ' + s.log_error);
    $('banner').hidden = !problems.length;
    text('banner', problems.join(' — '));

    // speed
    html('dec', unit(r0 && r0.decode_tps, 't/s', 1));
    html('pre', unit(r0 && r0.prefill_tps, 't/s', 0));
    spark('c-speed', s.requests.slice(0, 40).map(function (r) { return r.decode_tps; }).reverse());

    // GPUs
    var g = h.gpus, sm = s.samples;
    var avg = function (k) { var v = g.map(function (x) { return x[k]; }).filter(function (x) { return x != null; }); return v.length ? v.reduce(function (a, b) { return a + b; }, 0) / v.length : null; };
    var sum = function (k) { var v = g.map(function (x) { return x[k]; }).filter(function (x) { return x != null; }); return v.length ? v.reduce(function (a, b) { return a + b; }, 0) : null; };
    var maxv = function (k) { var v = g.map(function (x) { return x[k]; }).filter(function (x) { return x != null; }); return v.length ? Math.max.apply(null, v) : null; };
    var per = function (k, fmt) { return g.length > 1 ? g.map(function (x) { return x.name + ' ' + fmt(x[k]); }).join(' · ') : ''; };
    glide('gpu', avg('busy'), function (v) { html('gpu', unit(v, '%')); });
    text('gpu-sub', g.length ? (per('busy', function (v) { return num(v) + '%'; }) || g[0].name + (g[0].clock ? ' · ' + f0.format(g[0].clock) + ' MHz' : '')) : 'no GPU sensor');
    spark('c-gpu', sm.map(function (x) { return x.gpu; }), 100);
    var mu = sum('mem_used'), mt = sum('mem_total');
    glide('gmem', mu, function (v) { html('gmem', v == null ? '–' : f1.format(gib(v)) + '<small>/ ' + f0.format(gib(mt)) + ' GB</small>'); });
    text('gmem-sub', mu == null ? 'not exposed' : 'VRAM + GTT · ' + f0.format(pct(mu, mt)) + ' %');
    spark('c-gmem', sm.map(function (x) { return x.gpu_mem; }), mt || undefined);
    glide('temp', maxv('temp'), function (v) { html('temp', unit(v, '°C')); });
    text('temp-sub', per('temp', function (v) { return num(v) + '°'; }) || (g.length ? 'edge sensor' : ''));
    spark('c-temp', sm.map(function (x) { return x.temp; }), 100);
    var pw = sum('power'), cap = sum('power_cap');
    glide('pow', pw, function (v) { html('pow', unit(v, 'W')); });
    text('pow-sub', cap ? 'of ' + f0.format(cap) + ' W limit' : g.length ? 'GPU package power' : '');
    spark('c-pow', sm.map(function (x) { return x.power; }));

    // KV pool
    var used = m.kv_cache_tokens, cap2 = m.kv_pool_positions;
    glide('pool', m.kv_cache_usage_ratio != null ? 100 * m.kv_cache_usage_ratio : null, function (v) { html('pool', unit(v, '%')); });
    text('pool-sub', used != null && cap2 ? kfmt(used) + ' / ' + kfmt(cap2) + ' positions, all slots' : '');
    spark('c-pool', sm.map(function (x) { return x.pool; }), 100);

    // CPU / RAM
    glide('cpu', h.cpu, function (v) { html('cpu', unit(v, '%')); });
    text('cpu-sub', h.threads ? h.threads + ' threads' : '');
    spark('c-cpu', sm.map(function (x) { return x.cpu; }), 100);
    glide('ram', h.mem_used, function (v) { html('ram', v == null ? '–' : f1.format(gib(v)) + '<small>/ ' + f0.format(gib(h.mem_total)) + ' GB</small>'); });
    text('ram-sub', h.mem_used == null ? '' : f0.format(pct(h.mem_used, h.mem_total)) + ' % used (weights are locked in RAM)');
    spark('c-ram', sm.map(function (x) { return x.mem; }), h.mem_total || undefined);

    // context gauge
    var ctx = s.model.context, lastCtx = r0 ? r0.prompt + r0.output : null, p = pct(lastCtx, ctx);
    $('g-arc').style.strokeDasharray = (p == null ? 0 : Math.min(100, p) * 0.75) + ' 100';
    glide('ctx', p, function (v) { text('g-pct', v == null ? '–' : f0.format(v) + ' %'); });
    text('g-sub', lastCtx == null ? '' : kfmt(lastCtx) + (ctx ? ' / ' + kfmt(ctx) : ''));

    // stats
    var hits = m.cache_hits_total, miss = m.cache_misses_total, hr = pct(hits, (hits || 0) + (miss || 0));
    text('s-cache', hr == null ? '–' : f1.format(hr) + ' %'); setW('m-cache', hr);
    // prompt_tokens_total counts the tokens actually read (not the reused ones)
    var reuse = pct(m.prompt_tokens_cached_total, (m.prompt_tokens_cached_total || 0) + (m.prompt_tokens_total || 0));
    text('s-reuse', reuse == null ? '–' : f1.format(Math.min(100, reuse)) + ' %'); setW('m-reuse', reuse);
    var dr = pct(m.draft_tokens_accepted_total, m.draft_tokens_total);
    text('s-draft', dr == null ? '–' : f1.format(dr) + ' %'); setW('m-draft', dr);
    var adec = m.tokens_predicted_seconds_total ? m.tokens_predicted_total / m.tokens_predicted_seconds_total : null;
    var apre = m.prompt_seconds_total ? m.prompt_tokens_total / m.prompt_seconds_total : null;
    text('s-adec', adec == null ? '–' : f1.format(adec) + ' t/s');
    text('s-apre', apre == null ? '–' : f0.format(apre) + ' t/s');
    text('s-req', m.requests_total == null ? '–' : f0.format(m.requests_total));
    text('s-gen', kfmt(m.tokens_predicted_total));
    text('s-disk', m.cache_disk_bytes == null ? '–' : f1.format(gib(m.cache_disk_bytes)) + (m.cache_disk_budget_bytes ? ' / ' + f0.format(gib(m.cache_disk_budget_bytes)) : '') + ' GB');
    text('s-disc', f0.format(s.disconnects));

    // table
    var rows = s.requests.map(function (r) {
      return '<tr><td>' + clock(r.t) + '</td><td class="n">' + f0.format(r.prompt) + '</td><td class="n">' + f0.format(r.cached) +
        '</td><td class="n">' + f0.format(r.output) + '</td><td class="n">' + num(r.prefill_tps) +
        '</td><td class="n good">' + f1.format(r.decode_tps) + '</td><td class="n">' +
        (r.think == null ? '–' : '<span class="badge' + (r.think ? ' on' : '') + '">' + (r.think ? 'on' : 'off') + '</span>') +
        '</td><td class="n">' + (r.pool_used != null && r.pool_total ? f0.format(pct(r.pool_used, r.pool_total)) + ' %' : '–') +
        '</td><td class="n">' + dur(r.duration_s) + '</td></tr>';
    });
    var body = rows.length ? rows.join('') : '<tr><td colspan="9" class="muted">No finished request in the log yet.</td></tr>';
    if (body !== prevRows) { html('rows', body); prevRows = body; }
    // summary since start
    var since = s.started ? 'Since ' + new Date(s.started * 1000).toLocaleString(loc, { weekday: 'short', hour: '2-digit', minute: '2-digit' }) : 'Since Halogen started';
    text('summary', m.requests_total == null ? '' : since + ': ' + f0.format(m.requests_total) + ' requests · ' +
      f0.format(m.prompt_tokens_total || 0) + ' prompt tokens read' + (apre ? ' at ' + f0.format(apre) + ' tok/s' : '') +
      (m.prompt_tokens_cached_total ? ' (' + f0.format(m.prompt_tokens_cached_total) + ' reused)' : '') + ' · ' +
      f0.format(m.tokens_predicted_total || 0) + ' written' + (adec ? ' at ' + f1.format(adec) + ' tok/s' : ''));

    // prompt cache
    var c = s.cache || {};
    text('pc-reused', s.requests_kept ? f0.format(s.requests_reused) + ' of ' + f0.format(s.requests_kept) + ' requests reused part of their prompt' : '');
    text('pc-entries', c.entries == null ? '–' : f0.format(c.entries) + (c.max_entries ? ' / ' + f0.format(c.max_entries) : ''));
    setW('pm-entries', pct(c.entries, c.max_entries));
    text('pc-disk', c.disk_bytes == null ? (c.disk_on === 0 ? 'off' : '–') : f1.format(gib(c.disk_bytes)) + (c.disk_budget_bytes ? ' / ' + f0.format(gib(c.disk_budget_bytes)) : '') + ' GB');
    setW('pm-disk', pct(c.disk_bytes, c.disk_budget_bytes));
    text('pc-last', r0 ? f0.format(r0.cached) + ' of ' + f0.format(r0.prompt) + ' prompt tokens reused' : '–');
    text('pc-saved', c.prompt_tokens_saved == null ? '–' : f0.format(c.prompt_tokens_saved) + ' tokens' + (c.token_hit_rate != null ? ' (' + f0.format(100 * c.token_hit_rate) + ' % of all prompt tokens)' : ''));
    text('pc-hits', c.hits == null ? '–' : f0.format(c.hits) + ' / ' + f0.format(c.misses || 0) + (c.hit_rate != null ? ' · ' + f1.format(100 * c.hit_rate) + ' % hit rate' : ''));
    text('pc-stores', c.stores == null ? '–' : f0.format(c.stores) + ' / ' + f0.format(c.evicted || 0));
    text('pc-ram', c.bytes == null ? '–' : f1.format(gib(c.bytes)) + ' GB' + (c.last_entry_bytes ? ' · last entry ' + f0.format(c.last_entry_bytes / 1048576) + ' MB' : ''));
    text('pc-restore', c.restore_ms_total != null && c.hits ? f0.format(c.restore_ms_total / c.hits) + ' ms on average' : '–');
    text('pc-records', c.disk_records == null ? '–' : f0.format(c.disk_records) + ' in ' + f0.format(c.disk_lineages || 0) + ' lineages');
    text('pc-dhits', c.disk_hits == null ? '–' : f0.format(c.disk_hits) + ' / ' + f0.format(c.disk_misses || 0));

    text('show-all', showAll ? 'Show recent' : 'Show all (' + f0.format(s.requests_kept) + ')');

    text('foot', 'halogen-ui ' + s.ui_version + (s.model.version ? ' · Halogen ' + s.model.version : '') +
      (s.model.name ? ' · ' + s.model.name : '') + ' · refreshed ' + clock(s.now));
  }

  // The server sends only the graph points newer than the last one we have; the page keeps the history itself.
  var hist = [], MAX_HIST = 600;
  function tick() {
    if (busy || document.hidden) return;
    busy = true;
    var q = [];
    if (showAll) q.push('all=1');
    if (hist.length) q.push('since=' + hist[hist.length - 1].t);
    fetch('api/state' + (q.length ? '?' + q.join('&') : ''), { cache: 'no-store' })
      .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
      .then(function (s) {
        failures = 0;
        hist = hist.concat(s.samples).slice(-MAX_HIST);
        s.samples = hist;
        last = s; render(s);
      })
      .catch(function () {
        failures++;
        if (failures > 1) { $('banner').hidden = false; text('banner', 'The monitor itself does not answer (' + failures + ' tries).'); }
      })
      .then(function () { busy = false; });
  }
  window.addEventListener('resize', function () { if (last) render(last); });
  document.addEventListener('visibilitychange', tick);
  tick();
  setInterval(tick, 500);
})();
