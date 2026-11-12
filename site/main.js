/* Own Backup — the one-page site: language, motion, live release data.
   No dependencies. English is the text in index.html; Russian is in i18n.js. */
(function () {
  'use strict';

  var $ = function (s, r) { return (r || document).querySelector(s); };
  var $$ = function (s, r) { return Array.prototype.slice.call((r || document).querySelectorAll(s)); };
  var REPO = 'coinman-dev/own-backup';
  var RU = window.OB_RU || {};
  var EN = {
    'meta.title': document.title,
    'meta.desc': ($('meta[name="description"]') || {}).content || '',
    'how.cv.snap': 'snapshot',
    'how.cv.verified': 'verified',
    'sc.c.backup-dashboard': 'Jobs, the last backup of each and what is in it by file type.',
    'sc.c.backup-progress': 'A run: the phase, the speed, the blocks done and the time left.',
    'sc.c.wizard-source': 'The job wizard: volumes or a whole disk as the source.',
    'sc.c.wizard-scheme': 'The scheme: image format, backup method and retention.',
    'sc.c.wizard-exclusions': 'Exclusion masks, separate for Windows and Linux filesystems.',
    'sc.c.restore': 'Restore: the disk as it was in the backup, version by version.',
    'sc.c.boot-menu': 'The UEFI boot entries: what the firmware offers and where it leads.',
    'sc.c.tools': 'Mount a backup as a disk, validate an image, build the boot medium.',
    'sc.c.cleanup': 'Cleanup after an analysis: every item with its size.',
    'sc.c.activity': 'Activity: the history of backups, restores and scheduled checks.',
    'term.1': '# The configured jobs, with their ids.',
    'term.2': '# Queues a job, like “Back up now”.',
    'term.3': '# Validates an image (with its chain) against its checksums.',
    'term.4': '# The UEFI boot entries: what the firmware offers and where it leads.',
    'unit.mb': 'MB'
  };
  var lang = 'en';
  var reduced = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  var langHooks = [];

  function t(key) {
    if (lang === 'ru' && RU[key] != null) return RU[key];
    return EN[key] != null ? EN[key] : key;
  }

  /* ───────────── language ───────────── */

  var originals = new Map();
  function original(el, slot, read) {
    var rec = originals.get(el);
    if (!rec) { rec = {}; originals.set(el, rec); }
    if (!(slot in rec)) rec[slot] = read();
    return rec[slot];
  }

  function applyLang(next, remember) {
    lang = next === 'ru' ? 'ru' : 'en';
    var ru = lang === 'ru';
    document.documentElement.lang = lang;

    $$('[data-i18n]').forEach(function (el) {
      var en = original(el, 'html', function () { return el.innerHTML; });
      var v = ru ? RU[el.dataset.i18n] : null;
      el.innerHTML = v != null ? v : en;
    });
    [['i18nAlt', 'alt'], ['i18nAria', 'aria-label']].forEach(function (p) {
      $$('[data-' + p[0].replace(/[A-Z]/g, function (c) { return '-' + c.toLowerCase(); }) + ']').forEach(function (el) {
        var en = original(el, p[1], function () { return el.getAttribute(p[1]) || ''; });
        var v = ru ? RU[el.dataset[p[0]]] : null;
        el.setAttribute(p[1], v != null ? v : en);
      });
    });

    document.title = t('meta.title');
    var md = $('meta[name="description"]');
    if (md) md.content = t('meta.desc');

    $$('img[data-shot]').forEach(function (img) {
      var src = 'img/' + lang + '/' + img.dataset.shot + '.png';
      if (img.getAttribute('src') !== src) img.src = src;
    });
    $$('a[data-doc]').forEach(function (a) {
      a.href = 'https://github.com/' + REPO + '/blob/main/doc/' + lang + '/' + a.dataset.doc;
    });
    $$('a[data-href-ru]').forEach(function (a) {
      var en = original(a, 'href', function () { return a.getAttribute('href'); });
      a.href = ru ? a.dataset.hrefRu : en;
    });
    $$('.lang button').forEach(function (b) {
      b.setAttribute('aria-pressed', String(b.dataset.lang === lang));
    });

    if (remember) { try { localStorage.setItem('ob-lang', lang); } catch (e) { /* private mode */ } }
    langHooks.forEach(function (fn) { fn(); });
  }

  function initialLang() {
    var q = /[?&]lang=(ru|en)\b/.exec(location.search);
    if (q) return q[1];
    try { var s = localStorage.getItem('ob-lang'); if (s === 'ru' || s === 'en') return s; } catch (e) { /* ignore */ }
    var list = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language || 'en'];
    return /^ru\b/i.test(list[0]) ? 'ru' : 'en';
  }

  $$('.lang button').forEach(function (b) {
    b.addEventListener('click', function () { applyLang(b.dataset.lang, true); });
  });

  /* ───────────── small helpers ───────────── */

  function onVisible(el, cb, opts) {
    if (!('IntersectionObserver' in window)) { cb(true); return; }
    new IntersectionObserver(function (es) {
      es.forEach(function (e) { cb(e.isIntersecting); });
    }, opts || { threshold: 0.05 }).observe(el);
  }
  function animate(el, tick, opts) {
    var inView = false, raf = 0;
    function loop(now) {
      raf = 0;
      if (!inView || document.hidden) return;
      raf = requestAnimationFrame(loop);
      tick(now);
    }
    function kick() { if (inView && !document.hidden && !raf) raf = requestAnimationFrame(loop); }
    onVisible(el, function (v) { inView = v; kick(); }, opts);
    document.addEventListener('visibilitychange', kick);
  }
  function clamp(v, a, b) { return v < a ? a : v > b ? b : v; }
  function lerp(a, b, k) { return a + (b - a) * k; }
  function ease(k) { return k < 0.5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2; }
  function fitCanvas(cv) {
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    var w = cv.clientWidth, h = cv.clientHeight;
    if (cv.width !== Math.round(w * dpr) || cv.height !== Math.round(h * dpr)) {
      cv.width = Math.round(w * dpr);
      cv.height = Math.round(h * dpr);
    }
    var ctx = cv.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return { w: w, h: h, ctx: ctx };
  }
  function rr(ctx, x, y, w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }

  /* ───────────── nav, progress, scroll spy ───────────── */

  (function () {
    var nav = $('#nav'), bar = $('#progressBar'), ticking = false;
    function upd() {
      ticking = false;
      var y = window.scrollY || 0;
      var max = document.documentElement.scrollHeight - window.innerHeight;
      nav.classList.toggle('is-stuck', y > 12);
      bar.style.transform = 'scaleX(' + (max > 0 ? clamp(y / max, 0, 1) : 0) + ')';
    }
    window.addEventListener('scroll', function () {
      if (!ticking) { ticking = true; requestAnimationFrame(upd); }
    }, { passive: true });
    window.addEventListener('resize', upd);
    upd();

    var links = {};
    $$('.nav__links a').forEach(function (a) { links[a.getAttribute('href').slice(1)] = a; });
    if ('IntersectionObserver' in window) {
      var io = new IntersectionObserver(function (es) {
        es.forEach(function (e) {
          if (!e.isIntersecting) return;
          Object.keys(links).forEach(function (id) { links[id].classList.toggle('is-here', id === e.target.id); });
        });
      }, { rootMargin: '-45% 0px -50% 0px' });
      $$('main section[id]').forEach(function (s) { io.observe(s); });
    }
  })();

  /* ───────────── reveal on scroll, count-up ───────────── */

  (function () {
    $$('[data-stagger]').forEach(function (box) {
      $$('.reveal', box).forEach(function (el, i) { el.style.setProperty('--d', (i % 4) * 80 + 'ms'); });
    });
    var items = $$('.reveal');
    function show(el) {
      el.classList.add('is-in');
      // The stagger delay is for the entrance only: drop it so hover reacts at once.
      setTimeout(function () { el.style.setProperty('--d', '0ms'); }, 1300);
    }
    if (!('IntersectionObserver' in window)) { items.forEach(show); return; }
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) {
        if (e.isIntersecting) { show(e.target); io.unobserve(e.target); }
      });
    }, { threshold: 0.1, rootMargin: '0px 0px -6% 0px' });
    items.forEach(function (el) { io.observe(el); });
  })();

  $$('[data-count]').forEach(function (el) {
    var to = +el.dataset.count, done = false;
    if (reduced) return;
    el.textContent = '0';
    onVisible(el, function (vis) {
      if (!vis || done) return;
      done = true;
      var t0 = performance.now();
      (function step(now) {
        var k = clamp((now - t0) / 1300, 0, 1);
        el.textContent = String(Math.round(to * (1 - Math.pow(1 - k, 3))));
        if (k < 1) requestAnimationFrame(step);
      })(t0);
    }, { threshold: 0.6 });
  });

  /* ───────────── marquee: a second copy makes the loop seamless ───────────── */

  (function () {
    var m = $('#marquee');
    if (!m || reduced) return;
    $$('span', m).forEach(function (s) { m.appendChild(s.cloneNode(true)); });
    m.style.paddingRight = '14px';
  })();

  /* ───────────── hero: glowing cells of the block grid ───────────── */

  (function () {
    var cv = $('#heroCells');
    if (!cv || reduced) return;
    var G = 32, live = [], last = 0, spawnAt = 0;
    var ptr = null, size = fitCanvas(cv);
    var hero = cv.parentElement;
    hero.addEventListener('pointermove', function (e) {
      var r = cv.getBoundingClientRect();
      ptr = { x: e.clientX - r.left, y: e.clientY - r.top };
    });
    hero.addEventListener('pointerleave', function () { ptr = null; });
    window.addEventListener('resize', function () { size = fitCanvas(cv); });

    function spawn(now) {
      var cols = Math.ceil(size.w / G), rows = Math.ceil(size.h / G), c, r;
      if (ptr && Math.random() < 0.55) {
        c = Math.round(ptr.x / G + (Math.random() - 0.5) * 7);
        r = Math.round(ptr.y / G + (Math.random() - 0.5) * 7);
      } else {
        c = Math.floor(Math.random() * cols);
        r = Math.floor(Math.random() * rows);
      }
      live.push({ c: c, r: r, t0: now, life: 1600 + Math.random() * 2400, hot: Math.random() < 0.18 });
    }
    function frame(now) {
      if (now - last < 33) return;
      last = now;
      if (now > spawnAt) { spawn(now); spawnAt = now + (ptr ? 45 : 110); }
      var ctx = size.ctx;
      ctx.clearRect(0, 0, size.w, size.h);
      for (var i = live.length - 1; i >= 0; i--) {
        var p = live[i], k = (now - p.t0) / p.life;
        if (k >= 1) { live.splice(i, 1); continue; }
        var a = Math.sin(Math.PI * k) * (p.hot ? 0.3 : 0.2);
        ctx.fillStyle = p.hot ? 'rgba(255,138,43,' + a + ')' : 'rgba(124,110,245,' + a + ')';
        ctx.fillRect(p.c * G + 1, p.r * G + 1, G - 1, G - 1);
      }
    }
    animate(hero, frame);
  })();

  /* ───────────── tilt and card spotlight ───────────── */

  (function () {
    if (reduced || !window.matchMedia('(hover: hover)').matches) return;
    function tilt(zone, target, ry0, rx0, kx, ky) {
      zone.addEventListener('pointermove', function (e) {
        var r = zone.getBoundingClientRect();
        var nx = (e.clientX - r.left) / r.width - 0.5, ny = (e.clientY - r.top) / r.height - 0.5;
        target.style.setProperty('--ry', (ry0 + nx * kx).toFixed(2) + 'deg');
        target.style.setProperty('--rx', (rx0 - ny * ky).toFixed(2) + 'deg');
      });
      zone.addEventListener('pointerleave', function () {
        target.style.removeProperty('--ry');
        target.style.removeProperty('--rx');
      });
    }
    var hero = $('.hero'), shot = $('.shot3d');
    if (hero && shot) tilt(hero, shot, -13, 5, 12, 8);
    $$('[data-tilt]').forEach(function (el) { tilt(el.parentElement, el, -7, 3, 12, 8); });

    document.addEventListener('pointermove', function (e) {
      var c = e.target.closest && e.target.closest('.card, .dl, .fmt');
      if (!c) return;
      var r = c.getBoundingClientRect();
      c.style.setProperty('--mx', (e.clientX - r.left) + 'px');
      c.style.setProperty('--my', (e.clientY - r.top) + 'px');
    }, { passive: true });
  })();

  /* ───────────── the pipeline: disk → snapshot → zstd → AES → image ───────────── */

  (function () {
    var cv = $('#pipeCanvas');
    if (!cv) return;
    var stepsEl = $$('#pipeSteps li');
    var cRead = $('#cRead'), cFree = $('#cFree'), cExcl = $('#cExcl');
    var S, L, cells, slots, parts, phase, phaseT, head, headAcc, counts, shown = {}, fade, clock = 0, lastT = 0;
    var STEP_MS = 40, FLY_MS = 1500;
    var COL = { used: '111,96,234', excl: '236,96,96', info: '114,166,236', orange: '255,138,43', ok: '108,192,67' };

    function layout() {
      S = fitCanvas(cv);
      var W = S.w, H = S.h, vert = W < 560, pad = vert ? 16 : 26;
      var cols = vert ? 12 : 14, rows = 8;
      L = { vert: vert, cols: cols, rows: rows };
      if (vert) {
        var sw = W - pad * 2;
        L.s = Math.min(sw / cols, (H * 0.40) / rows);
        L.gx = (W - L.s * cols) / 2; L.gy = pad + 22;
        var gy = L.gy + L.s * rows;
        L.g1 = { x: W * 0.3, y: gy + (H - gy) * 0.17 };
        L.g2 = { x: W * 0.7, y: gy + (H - gy) * 0.17 };
        L.dcols = 16;
        L.dw = Math.min(sw, 360); L.dx = (W - L.dw) / 2; L.dy = gy + (H - gy) * 0.34 + 18;
        L.ds = L.dw / L.dcols;
      } else {
        var areaW = W * 0.42 - pad;
        L.s = Math.min(areaW / cols, 30);
        // As many rows as the height takes: the stage is as tall as the steps beside it.
        L.rows = rows = clamp(Math.floor((H - pad * 2 - 44) / L.s), 6, 14);
        L.gx = pad; L.gy = (H - L.s * rows) / 2 + 8;
        L.g1 = { x: W * 0.535, y: H / 2 + 8 };
        L.g2 = { x: W * 0.685, y: H / 2 + 8 };
        L.dcols = 8;
        L.dw = Math.min(W * 0.19, 210); L.dx = W - pad - L.dw;
        L.ds = L.dw / L.dcols;
        L.dy = 0; // set in reset(), when the row count is known
      }
    }

    function reset() {
      layout();
      cells = []; parts = []; slots = [];
      var n = L.cols * L.rows, i = 0, used = 0;
      while (i < n) {
        var r = Math.random(), kind = r < 0.56 ? 'used' : r < 0.9 ? 'free' : 'excl';
        var run = 1 + Math.floor(Math.random() * (kind === 'excl' ? 3 : 6));
        for (var k = 0; k < run && i < n; k++, i++) {
          cells.push({ kind: kind, c: i % L.cols, r: Math.floor(i / L.cols), seen: 0, sent: 0 });
          if (kind === 'used') used++;
        }
      }
      var drows = Math.ceil(used / L.dcols);
      if (!L.vert) {
        while (drows * L.ds > S.h - 100 && L.dcols < 16) {
          L.dcols++; L.ds = L.dw / L.dcols; drows = Math.ceil(used / L.dcols);
        }
        L.dy = (S.h - drows * L.ds) / 2 + 8;
      }
      L.drows = drows;
      STEP_MS = clamp(9000 / n, 36, 80);
      for (var j = 0; j < used; j++) slots.push({ c: j % L.dcols, r: Math.floor(j / L.dcols), full: 0, ok: 0 });
      counts = { read: 0, free: 0, excl: 0, sent: 0 };
      head = -1; headAcc = 0; fade = 1;
      setPhase('idle', clock);
      syncCounters();
    }

    function setPhase(p, now) {
      phase = p; phaseT = now;
      var on = p === 'snap' ? [1] : p === 'copy' ? [2, 3, 4] : (p === 'verify' || p === 'hold') ? [5] : [];
      stepsEl.forEach(function (li) { li.classList.toggle('is-on', on.indexOf(+li.dataset.step) >= 0); });
    }
    function syncCounters() {
      if (shown.read !== counts.read) { cRead.textContent = shown.read = counts.read; }
      if (shown.free !== counts.free) { cFree.textContent = shown.free = counts.free; }
      if (shown.excl !== counts.excl) { cExcl.textContent = shown.excl = counts.excl; }
    }
    function cellXY(c) { return { x: L.gx + (c.c + 0.5) * L.s, y: L.gy + (c.r + 0.5) * L.s }; }
    function slotXY(s) { return { x: L.dx + (s.c + 0.5) * L.ds, y: L.dy + (s.r + 0.5) * L.ds }; }

    function advance(now, dt) {
      var el = now - phaseT;
      if (phase === 'idle' && el > 900) setPhase('snap', now);
      else if (phase === 'snap' && el > 1500) setPhase('copy', now);
      else if (phase === 'copy') {
        headAcc += dt;
        while (headAcc >= STEP_MS && head < cells.length - 1) {
          headAcc -= STEP_MS;
          var c = cells[++head];
          c.seen = now;
          if (c.kind === 'free') counts.free++;
          else if (c.kind === 'excl') counts.excl++;
          else { c.sent = now; parts.push({ from: cellXY(c), slot: counts.sent++, t0: now }); }
        }
        if (head >= cells.length - 1 && !parts.length) setPhase('verify', now);
      } else if (phase === 'verify') {
        var k = clamp(el / 1500, 0, 1), upto = Math.floor(k * slots.length);
        for (var i = 0; i < upto; i++) if (!slots[i].ok) slots[i].ok = now;
        if (k >= 1) setPhase('hold', now);
      } else if (phase === 'hold' && el > 2600) setPhase('fade', now);
      else if (phase === 'fade') {
        fade = 1 - clamp(el / 600, 0, 1);
        if (el > 650) reset();
      }
      if (phase === 'idle') fade = clamp(el / 500, 0, 1);

      for (var p = parts.length - 1; p >= 0; p--) {
        if (now - parts[p].t0 >= FLY_MS) {
          slots[parts[p].slot].full = now;
          counts.read++;
          parts.splice(p, 1);
        }
      }
      syncCounters();
    }

    function draw(now) {
      var ctx = S.ctx, W = S.w, H = S.h, s = L.s, gap = Math.max(1.5, s * 0.14), ds = L.ds, dgap = Math.max(1, ds * 0.16);
      ctx.clearRect(0, 0, W, H);
      ctx.globalAlpha = fade;
      ctx.font = '500 12px "Cascadia Code","JetBrains Mono",ui-monospace,Consolas,monospace';
      ctx.textBaseline = 'alphabetic';

      var snapK = phase === 'snap' ? clamp((now - phaseT) / 1200, 0, 1) : (phase === 'idle' ? 0 : 1);
      var gw = L.cols * s, gh = L.rows * s;

      // the wire: source → zstd → AES → image
      ctx.strokeStyle = 'rgba(255,255,255,0.10)';
      ctx.lineWidth = 1;
      ctx.setLineDash([3, 5]);
      ctx.beginPath();
      if (L.vert) {
        ctx.moveTo(W / 2, L.gy + gh + 6); ctx.lineTo(L.g1.x, L.g1.y); ctx.lineTo(L.g2.x, L.g2.y); ctx.lineTo(W / 2, L.dy - 22);
      } else {
        ctx.moveTo(L.gx + gw + 10, L.g1.y); ctx.lineTo(L.dx - 12, L.g2.y);
      }
      ctx.stroke();
      ctx.setLineDash([]);

      // source frame and captions
      rr(ctx, L.gx - 7, L.gy - 7, gw + 14, gh + 14, 10);
      ctx.strokeStyle = snapK > 0 ? 'rgba(' + COL.info + ',' + (0.25 + 0.45 * snapK) + ')' : 'rgba(255,255,255,0.12)';
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.fillStyle = 'rgba(139,139,162,1)';
      ctx.textAlign = 'left';
      ctx.fillText('C:  NTFS', L.gx - 6, L.gy - 16);
      if (snapK > 0) {
        ctx.fillStyle = 'rgba(' + COL.info + ',' + snapK + ')';
        ctx.textAlign = 'right';
        ctx.fillText('❄ ' + t('how.cv.snap'), L.gx + gw + 6, L.gy - 16);
      }

      // cells
      for (var i = 0; i < cells.length; i++) {
        var c = cells[i], x = L.gx + c.c * s + gap / 2, y = L.gy + c.r * s + gap / 2, w = s - gap;
        var rad = Math.min(3, w * 0.22);
        if (c.kind === 'free') {
          rr(ctx, x + 0.5, y + 0.5, w - 1, w - 1, rad);
          ctx.strokeStyle = 'rgba(255,255,255,' + (c.seen ? 0.05 : 0.11) + ')';
          ctx.stroke();
        } else if (c.kind === 'used') {
          var a = c.sent ? lerp(0.9, 0.2, clamp((now - c.sent) / 500, 0, 1)) : 0.85;
          rr(ctx, x, y, w, w, rad);
          ctx.fillStyle = 'rgba(' + COL.used + ',' + a + ')';
          ctx.fill();
        } else {
          var fl = c.seen ? clamp(1 - (now - c.seen) / 700, 0, 1) : 0;
          rr(ctx, x, y, w, w, rad);
          ctx.fillStyle = 'rgba(' + COL.excl + ',' + (c.seen ? 0.06 + fl * 0.6 : 0.2) + ')';
          ctx.fill();
          ctx.save();
          ctx.clip();
          ctx.strokeStyle = 'rgba(' + COL.excl + ',' + (c.seen ? 0.25 + fl * 0.6 : 0.75) + ')';
          ctx.lineWidth = 1;
          ctx.beginPath();
          for (var h = -w; h < w; h += 4) { ctx.moveTo(x + h, y + w); ctx.lineTo(x + h + w, y); }
          ctx.stroke();
          ctx.restore();
        }
      }

      // the snapshot sweep
      if (phase === 'snap' && snapK < 1) {
        var sx = L.gx - 7 + (gw + 14) * ease(snapK);
        var g = ctx.createLinearGradient(sx - 60, 0, sx, 0);
        g.addColorStop(0, 'rgba(' + COL.info + ',0)');
        g.addColorStop(1, 'rgba(' + COL.info + ',0.30)');
        ctx.fillStyle = g;
        ctx.fillRect(Math.max(L.gx - 7, sx - 60), L.gy - 7, Math.min(60, sx - L.gx + 7), gh + 14);
        ctx.fillStyle = 'rgba(' + COL.info + ',0.95)';
        ctx.fillRect(sx - 1, L.gy - 7, 2, gh + 14);
      }

      // the reading head
      if (phase === 'copy' && head >= 0 && head < cells.length - 1) {
        var hc = cells[head];
        ctx.fillStyle = 'rgba(255,255,255,0.035)';
        ctx.fillRect(L.gx, L.gy + hc.r * s, gw, s);
        rr(ctx, L.gx + hc.c * s + 0.5, L.gy + hc.r * s + 0.5, s - 1, s - 1, 4);
        ctx.strokeStyle = 'rgba(255,255,255,0.9)';
        ctx.lineWidth = 1.5;
        ctx.stroke();
        ctx.lineWidth = 1;
      }

      // the image file
      var dh = L.drows * ds, done = counts.read / (slots.length || 1);
      rr(ctx, L.dx - 7, L.dy - 7, L.dw + 14, dh + 14, 10);
      ctx.strokeStyle = phase === 'hold' || phase === 'verify' ? 'rgba(' + COL.ok + ',0.55)' : 'rgba(' + COL.orange + ',' + (0.2 + 0.4 * done) + ')';
      ctx.stroke();
      ctx.fillStyle = 'rgba(139,139,162,1)';
      ctx.textAlign = 'left';
      ctx.fillText('image.obk', L.dx - 6, L.dy - 16);
      ctx.textAlign = 'right';
      if (phase === 'hold' || phase === 'verify') {
        ctx.fillStyle = 'rgba(' + COL.ok + ',1)';
        ctx.fillText(phase === 'hold' ? '✓ ' + t('how.cv.verified') : 'xxHash64…', L.dx + L.dw + 6, L.dy + dh + 24);
      }
      ctx.fillStyle = 'rgba(244,244,251,0.9)';
      ctx.fillText(Math.round(done * 100) + '%', L.dx + L.dw + 6, L.dy - 16);

      for (var j = 0; j < slots.length; j++) {
        var sl = slots[j], x2 = L.dx + sl.c * ds + dgap / 2, y2 = L.dy + sl.r * ds + dgap / 2, w2 = ds - dgap;
        rr(ctx, x2, y2, w2, w2, Math.min(2.5, w2 * 0.22));
        if (!sl.full) {
          ctx.strokeStyle = 'rgba(255,255,255,0.07)';
          ctx.stroke();
        } else if (sl.ok) {
          var kk = clamp((now - sl.ok) / 450, 0, 1);
          ctx.fillStyle = 'rgba(' + COL.ok + ',' + lerp(1, 0.78, kk) + ')';
          ctx.fill();
          if (kk < 1) { ctx.fillStyle = 'rgba(255,255,255,' + (0.7 * (1 - kk)) + ')'; ctx.fill(); }
        } else {
          var land = clamp((now - sl.full) / 350, 0, 1);
          ctx.fillStyle = 'rgba(' + COL.orange + ',0.88)';
          ctx.fill();
          if (land < 1) { ctx.fillStyle = 'rgba(255,255,255,' + (0.8 * (1 - land)) + ')'; ctx.fill(); }
        }
      }

      // blocks in flight
      for (var p = 0; p < parts.length; p++) {
        var pt = parts[p], k = clamp((now - pt.t0) / FLY_MS, 0, 1), to = slotXY(slots[pt.slot]);
        var px, py, size, mix;
        if (k < 0.38) {
          var e1 = ease(k / 0.38);
          px = lerp(pt.from.x, L.g1.x, e1); py = lerp(pt.from.y, L.g1.y, L.vert ? e1 : e1 * e1);
          size = lerp(s - gap, (s - gap) * 0.9, e1); mix = 0;
        } else if (k < 0.58) {
          var e2 = (k - 0.38) / 0.2;
          px = lerp(L.g1.x, L.g2.x, e2); py = lerp(L.g1.y, L.g2.y, e2);
          size = lerp((s - gap) * 0.9, (s - gap) * 0.55, clamp(e2 * 3, 0, 1)); mix = clamp((e2 - 0.75) * 4, 0, 1);
        } else {
          var e3 = ease((k - 0.58) / 0.42);
          px = lerp(L.g2.x, to.x, e3); py = lerp(L.g2.y, to.y, L.vert ? e3 : 1 - (1 - e3) * (1 - e3));
          size = lerp((s - gap) * 0.55, ds - dgap, e3); mix = 1;
        }
        var rC = Math.round(lerp(124, 255, mix)), gC = Math.round(lerp(110, 138, mix)), bC = Math.round(lerp(245, 43, mix));
        ctx.shadowColor = 'rgba(' + rC + ',' + gC + ',' + bC + ',0.9)';
        ctx.shadowBlur = 6;
        ctx.fillStyle = 'rgb(' + rC + ',' + gC + ',' + bC + ')';
        rr(ctx, px - size / 2, py - size / 2, size, size, Math.min(3, size * 0.22));
        ctx.fill();
        ctx.shadowBlur = 0;
      }
      // gates
      var pulse1 = 0, pulse2 = 0;
      parts.forEach(function (p) {
        var k = (now - p.t0) / FLY_MS;
        pulse1 = Math.max(pulse1, 1 - Math.abs(k - 0.38) / 0.12);
        pulse2 = Math.max(pulse2, 1 - Math.abs(k - 0.58) / 0.12);
      });
      gate(ctx, L.g1, 'zstd', COL.used, clamp(pulse1, 0, 1));
      gate(ctx, L.g2, L.vert ? 'AES-256' : 'AES-256-GCM', COL.orange, clamp(pulse2, 0, 1));

      ctx.globalAlpha = 1;
    }

    function gate(ctx, g, label, col, pulse) {
      ctx.font = '600 12.5px "Cascadia Code","JetBrains Mono",ui-monospace,Consolas,monospace';
      var w = ctx.measureText(label).width + 26, h = 32;
      rr(ctx, g.x - w / 2, g.y - h / 2, w, h, 9);
      ctx.fillStyle = '#14131f';
      ctx.shadowColor = 'rgba(' + col + ',' + (0.9 * pulse) + ')';
      ctx.shadowBlur = 22 * pulse;
      ctx.fill();
      ctx.shadowBlur = 0;
      ctx.strokeStyle = 'rgba(' + col + ',' + (0.4 + 0.6 * pulse) + ')';
      ctx.lineWidth = 1 + pulse;
      ctx.stroke();
      ctx.lineWidth = 1;
      ctx.fillStyle = 'rgba(244,244,251,' + (0.8 + 0.2 * pulse) + ')';
      ctx.textAlign = 'center';
      ctx.fillText(label, g.x, g.y + 4.5);
      ctx.font = '500 12px "Cascadia Code","JetBrains Mono",ui-monospace,Consolas,monospace';
    }

    function settle() {
      // The finished picture, for visitors who asked for no motion.
      var now = clock - 5000;
      cells.forEach(function (c) {
        c.seen = now;
        if (c.kind === 'used') { c.sent = now; counts.read++; }
        else if (c.kind === 'free') counts.free++;
        else counts.excl++;
      });
      slots.forEach(function (s) { s.full = now; s.ok = now; });
      head = cells.length; fade = 1;
      phase = 'hold'; phaseT = clock;
      syncCounters();
      draw(clock);
    }

    function frame(now) {
      var dt = clamp(now - lastT, 0, 100);
      lastT = now;
      clock += dt;
      advance(clock, dt);
      draw(clock);
    }

    reset();
    if (reduced) {
      settle();
      window.addEventListener('resize', function () { reset(); settle(); });
      langHooks.push(function () { draw(clock); });
      return;
    }
    var rz = 0;
    window.addEventListener('resize', function () {
      clearTimeout(rz);
      rz = setTimeout(function () {
        if (cv.clientWidth !== S.w || cv.clientHeight !== S.h) reset();
      }, 150);
    });
    animate(cv, frame, { threshold: 0.15 });
  })();

  /* ───────────── versions and chains ───────────── */

  (function () {
    var root = $('#chain');
    if (!root) return;
    var row = $('#chainRow'), svg = $('#chainLinks'), stage = row.parentElement;
    var run = 0, uid = 0, vis = false, mode = 'inc', paths = {};
    var NS = 'http://www.w3.org/2000/svg';

    function sleep(ms) {
      return new Promise(function (res) {
        (function wait(left) {
          if (!vis) { setTimeout(function () { wait(left); }, 300); return; }
          setTimeout(res, left);
        })(ms);
      });
    }
    function add(kind, label, h, dep, instant) {
      var el = document.createElement('div');
      el.className = 'ver ver--' + kind + (instant ? '' : ' is-new');
      el.style.setProperty('--h', h + 'px');
      el.dataset.id = String(++uid);
      if (dep) el.dataset.dep = dep.dataset.id;
      el.innerHTML = '<span>' + label + '</span>';
      row.appendChild(el);
      if (!instant) { void el.offsetWidth; el.classList.remove('is-new'); }
      return el;
    }
    function drop(el, cls) {
      el.classList.add(cls || 'is-gone');
      el.dataset.dead = '1';
      setTimeout(function () { el.remove(); }, 700);
    }
    function scale() { return stage.clientHeight / 290; }
    function few() { return stage.clientWidth < 460; }

    function links() {
      var sr = stage.getBoundingClientRect(), seen = {};
      $$('.ver[data-dep]', row).forEach(function (el) {
        if (el.dataset.dead || el.classList.contains('is-new')) return;
        var dep = $('.ver[data-id="' + el.dataset.dep + '"]', row);
        if (!dep || dep.dataset.dead) return;
        var a = el.getBoundingClientRect(), b = dep.getBoundingClientRect();
        if (a.width < 8) return;
        var x1 = a.left + a.width / 2 - sr.left, y1 = a.top - sr.top - 5;
        var x2 = b.left + b.width / 2 - sr.left, y2 = b.top - sr.top - 5;
        var lift = Math.min(70, 16 + Math.abs(x1 - x2) * 0.16);
        var d = 'M' + x1.toFixed(1) + ' ' + y1.toFixed(1) + ' Q' + ((x1 + x2) / 2).toFixed(1) + ' ' + (Math.min(y1, y2) - lift).toFixed(1) + ' ' + x2.toFixed(1) + ' ' + y2.toFixed(1);
        var p = paths[el.dataset.id];
        if (!p) { p = paths[el.dataset.id] = document.createElementNS(NS, 'path'); svg.appendChild(p); }
        if (p.getAttribute('d') !== d) p.setAttribute('d', d);
        seen[el.dataset.id] = 1;
      });
      Object.keys(paths).forEach(function (id) {
        if (!seen[id]) { paths[id].remove(); delete paths[id]; }
      });
    }
    (function loop() {
      if (vis) links();
      requestAnimationFrame(loop);
    })();

    var scripts = {
      inc: async function (ok) {
        var prev = null, k = scale();
        for (var n = 1; ; n++) {
          var chain = [], last = add('full', n + '-full', (150 + Math.random() * 30) * k);
          chain.push(last);
          await sleep(750); if (!ok()) return;
          for (var i = 0; i < (few() ? 2 : 3); i++) {
            last = add('inc', n + '-inc', (38 + Math.random() * 40) * k, last);
            chain.push(last);
            await sleep(700); if (!ok()) return;
          }
          if (prev) { prev.forEach(function (el) { drop(el); }); await sleep(900); if (!ok()) return; }
          prev = chain;
          await sleep(500); if (!ok()) return;
        }
      },
      diff: async function (ok) {
        var k = scale();
        for (var n = 1; ; n++) {
          var all = [], full = add('full', n + '-full', (150 + Math.random() * 30) * k);
          all.push(full);
          await sleep(750); if (!ok()) return;
          for (var i = 0; i < (few() ? 4 : 5); i++) {
            all.push(add('diff', n + '-diff', (40 + i * 20 + Math.random() * 8) * k, full));
            await sleep(800); if (!ok()) return;
          }
          await sleep(1500); if (!ok()) return;
          all.forEach(function (el) { drop(el); });
          await sleep(800); if (!ok()) return;
        }
      },
      cons: async function (ok) {
        var k = scale(), h = 150, win = few() ? 3 : 4;
        var full = add('full', '1-full', h * k), incs = [], last = full;
        await sleep(700); if (!ok()) return;
        for (;;) {
          last = add('inc', '1-inc', (38 + Math.random() * 40) * k, last);
          incs.push(last);
          await sleep(800); if (!ok()) return;
          if (incs.length > win) {
            var old = incs.shift();
            drop(old, 'is-merge');
            if (incs[0]) incs[0].dataset.dep = full.dataset.id;
            h = Math.min(205, h + 5);
            full.style.setProperty('--h', h * k + 'px');
            full.classList.add('is-pulse');
            await sleep(700); if (!ok()) return;
            full.classList.remove('is-pulse');
            await sleep(500); if (!ok()) return;
          }
        }
      }
    };

    function still() {
      var k = scale(), full = add('full', '1-full', 165 * k, null, true), last = full;
      for (var i = 0; i < 4; i++) {
        var el = mode === 'diff' ? add('diff', '1-diff', (44 + i * 20) * k, full, true) : add('inc', '1-inc', (44 + (i * 37) % 30) * k, last, true);
        last = el;
      }
      links();
    }
    function play(m) {
      mode = m;
      var my = ++run;
      row.textContent = '';
      $$('.tabs button', root).forEach(function (b) { b.setAttribute('aria-selected', String(b.dataset.mode === m)); });
      $$('[data-mode-text]', root).forEach(function (p) { p.hidden = p.dataset.modeText !== m; });
      if (reduced) { still(); return; }
      scripts[m](function () { return my === run; });
    }
    $$('.tabs button', root).forEach(function (b) {
      b.addEventListener('click', function () { play(b.dataset.mode); });
    });
    onVisible(root, function (v) { vis = v; }, { threshold: 0.2 });
    if (reduced) vis = true;
    play('inc');
  })();

  /* ───────────── screens ───────────── */

  (function () {
    var root = $('#gallery');
    if (!root) return;
    var tabs = $$('[data-shot-tab]', root), cap = $('#galleryCap');
    var top = $('#galleryImg'), under = $('#galleryImg2');
    var idx = 0, elapsed = 0, DWELL = 6500, vis = false, hover = false, token = 0;
    root.style.setProperty('--dwell', DWELL + 'ms');

    function src(i) { return 'img/' + lang + '/' + tabs[i].dataset.shotTab + '.png'; }
    function label() {
      cap.textContent = t('sc.c.' + tabs[idx].dataset.shotTab);
      top.alt = tabs[idx].textContent + ' — ' + cap.textContent;
    }
    function show(i, instant) {
      idx = (i + tabs.length) % tabs.length;
      elapsed = 0;
      tabs.forEach(function (b, n) { b.setAttribute('aria-selected', String(n === idx)); });
      label();
      var my = ++token, next = src(idx);
      top.dataset.shot = tabs[idx].dataset.shotTab;
      if (instant) { top.src = next; return; }
      var pre = new Image();
      pre.onload = pre.onerror = function () {
        if (my !== token) return;
        // The new picture goes under, then the two swap roles.
        under.src = next;
        under.classList.remove('is-under');
        top.classList.add('is-under');
        var tmp = top; top = under; under = tmp;
        top.dataset.shot = tabs[idx].dataset.shotTab;
        delete under.dataset.shot;
        label();
      };
      pre.src = next;
    }
    tabs.forEach(function (b, n) { b.addEventListener('click', function () { show(n); }); });
    root.addEventListener('pointerenter', function () { hover = true; sync(); });
    root.addEventListener('pointerleave', function () { hover = false; sync(); });
    function sync() { root.classList.toggle('is-paused', hover || !vis); }
    onVisible(root, function (v) { vis = v; sync(); }, { threshold: 0.3 });

    tabs.forEach(function (b, n) { b.setAttribute('aria-selected', String(n === 0)); });
    langHooks.push(function () { top.src = src(idx); label(); });
    if (reduced) { root.classList.add('is-paused'); return; }
    setInterval(function () {
      if (hover || !vis || document.hidden) return;
      elapsed += 100;
      if (elapsed >= DWELL) show(idx + 1);
    }, 100);
  })();

  /* ───────────── terminal ───────────── */

  (function () {
    var body = $('#termBody');
    if (!body) return;
    var CMDS = [
      ['own-backup-cli jobs list', 'term.1'],
      ['own-backup-cli jobs run 3f2a…', 'term.2'],
      ['own-backup-cli verify "F:\\Backup\\Disk_C_Win11\\Disk_C_Win11_disk4_inc_20261024-185102.obk"', 'term.3'],
      ['own-backup-cli boot', 'term.4']
    ];
    var run = 0, vis = false, started = false;
    function esc(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;'); }
    function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }
    function whole() {
      body.innerHTML = CMDS.map(function (c) {
        return '<span class="p">$</span> ' + esc(c[0]) + '\n<span class="c">' + esc(t(c[1])) + '</span>';
      }).join('\n');
    }
    async function type() {
      var my = ++run, done = '';
      for (;;) {
        done = '';
        for (var i = 0; i < CMDS.length; i++) {
          var cmd = CMDS[i][0];
          for (var n = 0; n <= cmd.length; n++) {
            body.innerHTML = done + '<span class="p">$</span> ' + esc(cmd.slice(0, n)) + '<span class="caret"></span>';
            await sleep(n === 0 ? 500 : 16 + Math.random() * 34);
            if (my !== run) return;
            while (!vis) { await sleep(300); if (my !== run) return; }
          }
          await sleep(320); if (my !== run) return;
          done += '<span class="p">$</span> ' + esc(cmd) + '\n<span class="c">' + esc(t(CMDS[i][1])) + '</span>\n';
          body.innerHTML = done + '<span class="p">$</span> <span class="caret"></span>';
          await sleep(900); if (my !== run) return;
        }
        await sleep(7000); if (my !== run) return;
      }
    }
    if (reduced) { whole(); langHooks.push(whole); return; }
    onVisible(body, function (v) {
      vis = v;
      if (v && !started) { started = true; type(); }
    }, { threshold: 0.4 });
    langHooks.push(function () { if (started) type(); else whole(); });
  })();

  /* ───────────── copy button ───────────── */

  $$('[data-copy]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      var src = $(btn.dataset.copy);
      if (!src || !navigator.clipboard) return;
      navigator.clipboard.writeText(src.textContent.trim()).then(function () {
        btn.classList.add('is-done');
        setTimeout(function () { btn.classList.remove('is-done'); }, 1400);
      }, function () { /* no permission: nothing to say */ });
    });
  });

  /* ───────────── this visitor's system ───────────── */

  (function () {
    var p = ((navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '') + ' ' + navigator.userAgent;
    var os = /android|iphone|ipad/i.test(p) ? '' : /windows|win32|win64/i.test(p) ? 'win' : /linux|x11/i.test(p) ? 'linux' : '';
    if (!os) return;
    $$('.dl[data-os="' + os + '"]').forEach(function (el) { el.classList.add('is-mine'); });
    if (os === 'linux') {
      var w = $('.hero__cta [data-os="win"]'), l = $('.hero__cta [data-os="linux"]');
      if (w && l) {
        w.classList.replace('btn--go', 'btn--line');
        l.classList.replace('btn--line', 'btn--go');
        l.parentElement.insertBefore(l, w);
      }
    }
  })();

  /* ───────────── the current release, from the GitHub API ─────────────
     The links in index.html are the fallback: they work with no script, and
     stay if the API is unreachable or rate-limited. */

  (function () {
    var sizes = {};
    function paintSizes() {
      Object.keys(sizes).forEach(function (k) {
        $$('[data-asset-size="' + k + '"]').forEach(function (el) {
          el.textContent = Math.round(sizes[k] / 1048576) + ' ' + t('unit.mb');
        });
      });
    }
    langHooks.push(function () {
      $$('[data-asset-size]').forEach(function (el) {
        if (!sizes[el.dataset.assetSize]) el.textContent = el.textContent.replace(/\S+$/, t('unit.mb'));
      });
      paintSizes();
    });

    function apply(rel) {
      if (!rel || !rel.tag || !rel.assets) return;
      var a = rel.assets;
      if (!a.exe || !a.deb) return; // a release still being published
      $$('[data-version]').forEach(function (el) { el.textContent = rel.tag.replace(/^v/, ''); });
      $$('[data-release-link]').forEach(function (el) { el.href = rel.url; });
      Object.keys(a).forEach(function (k) {
        $$('a[data-asset="' + k + '"]').forEach(function (el) { el.href = a[k].url; });
        $$('[data-asset-name="' + k + '"]').forEach(function (el) { el.textContent = a[k].name; });
        if (a[k].size) sizes[k] = a[k].size;
      });
      var cmd = $('#debCmd');
      if (cmd) cmd.textContent = 'sudo apt install ./' + a.deb.name;
      paintSizes();
    }

    var cached = null;
    try { cached = JSON.parse(sessionStorage.getItem('ob-release') || 'null'); } catch (e) { /* ignore */ }
    if (cached && Date.now() - cached.at < 3600e3) { apply(cached); return; }
    if (!window.fetch) return;

    fetch('https://api.github.com/repos/' + REPO + '/releases?per_page=5', { headers: { Accept: 'application/vnd.github+json' } })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(r.status); })
      .then(function (list) {
        var rel = (list || []).filter(function (r) { return !r.draft; })[0];
        if (!rel) return;
        var out = { at: Date.now(), tag: rel.tag_name, url: rel.html_url, assets: {} };
        (rel.assets || []).forEach(function (x) {
          var k = /^own-backup-install.*\.exe$/i.test(x.name) ? 'exe' : /\.deb$/i.test(x.name) ? 'deb' : /\.iso$/i.test(x.name) ? 'iso' : /^SHA256SUMS/i.test(x.name) ? 'sums' : '';
          if (k) out.assets[k] = { name: x.name, url: x.browser_download_url, size: x.size };
        });
        apply(out);
        try { sessionStorage.setItem('ob-release', JSON.stringify(out)); } catch (e) { /* ignore */ }
      })
      .catch(function () { /* the built-in links stay */ });
  })();

  applyLang(initialLang(), false);
})();
