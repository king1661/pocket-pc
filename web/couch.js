// Injected into every page the TV shows. Turns D-pad presses into focus moves
// (spatial navigation) and tells the Rust side which trusted input to send.
(() => {
  if (window.__couch) return;
  const C = (window.__couch = { cur: null, navUntil: 0, quietUntil: 0, ringOn: false, raf: 0 });
  const PLAYER_NAV_MS = 8000;
  const SEL =
    'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],' +
    '[role=tab],[role=menuitem],[role=option],[role=checkbox],[role=switch],[role=radio],[role=slider],' +
    '[tabindex]:not([tabindex="-1"]),[onclick],[contenteditable=""],[contenteditable=true]';
  let style, ring, dot, dotTimer;

  function dom() {
    const fs = document.fullscreenElement;
    const host = fs && fs.tagName !== 'VIDEO' ? fs : document.body;
    if (!host) return false;
    if (!style) {
      style = document.createElement('style');
      style.textContent = `
        html.__couch_nomouse, html.__couch_nomouse * { cursor: none !important; }
        html, body { overscroll-behavior-x: none !important; }
        #__couch_ring { position: fixed; pointer-events: none; z-index: 2147483647; display: none;
          border: 4px solid #fff; border-radius: 12px; box-sizing: border-box;
          box-shadow: 0 0 0 3px rgba(10,132,255,.95), 0 0 28px 6px rgba(10,132,255,.55);
          transition: left .12s ease-out, top .12s ease-out, width .12s ease-out, height .12s ease-out; }
        #__couch_dot { position: fixed; pointer-events: none; z-index: 2147483647; display: none;
          width: 28px; height: 28px; margin: -14px 0 0 -14px; border-radius: 50%; box-sizing: border-box;
          border: 1px solid rgba(255,255,255,.55); background: rgba(205,205,212,.55);
          -webkit-backdrop-filter: blur(6px) saturate(160%); backdrop-filter: blur(6px) saturate(160%);
          box-shadow: 0 0 0 .5px rgba(0,0,0,.25), 0 2px 10px rgba(0,0,0,.3); transition: opacity .3s; }
        #__couch_dot.click { animation: __couch_click .35s ease-out; }
        @keyframes __couch_click { 0% { transform: scale(1); } 40% { transform: scale(.72); background: rgba(255,255,255,.85); } 100% { transform: scale(1); } }`;
      ring = document.createElement('div');
      ring.id = '__couch_ring';
      dot = document.createElement('div');
      dot.id = '__couch_dot';
    }
    if (!style.isConnected) (document.head || document.documentElement).appendChild(style);
    if (ring.parentNode !== host) host.appendChild(ring);
    if (dot.parentNode !== host) host.appendChild(dot);
    document.documentElement.classList.add('__couch_nomouse');
    return true;
  }

  const center = (r) => ({ x: r.left + r.width / 2, y: r.top + r.height / 2 });
  const overlaps = (a1, a2, b1, b2) => Math.min(a2, b2) - Math.max(a1, b1) > 0;

  function isInput(el) {
    if (!el) return false;
    if (el.isContentEditable || el.tagName === 'TEXTAREA') return true;
    return el.tagName === 'INPUT' && !/^(button|submit|reset|checkbox|radio|range|image|file|color)$/i.test(el.type);
  }

  // A video big enough to be "the show", on a page that looks like a player.
  function video() {
    const playerUrl = /\/(watch|video|play|player|live)\b/i.test(location.pathname);
    for (const v of document.querySelectorAll('video')) {
      const r = v.getBoundingClientRect();
      if (v.readyState === 0 || r.width < innerWidth * 0.5) continue;
      if (playerUrl || (document.fullscreenElement && document.fullscreenElement.contains(v))) return v;
    }
    return null;
  }

  const isHome = () => document.documentElement.hasAttribute('data-couch-home');
  const siteName = () => (isHome() ? 'home' : location.hostname.replace(/^www\./, ''));

  function candidates() {
    const root = document.documentElement;
    // Our cursor:none rule would hide every site's cursor:pointer, which we use to find clickables.
    root.classList.remove('__couch_nomouse');
    const vw = innerWidth, vh = innerHeight, out = [], seen = new Set();
    const add = (el) => {
      if (seen.has(el) || el === ring || el === dot) return;
      seen.add(el);
      if (el.disabled) return;
      const r = el.getBoundingClientRect();
      if (r.width < 8 || r.height < 8) return;
      if (r.right <= 0 || r.left >= vw || r.bottom < -vh * 1.5 || r.top > vh * 2.5) return;
      const s = getComputedStyle(el);
      if (s.visibility === 'hidden' || s.display === 'none' || +s.opacity < 0.05) return;
      const c = center(r);
      if (c.y > 0 && c.y < vh) {
        // Reject things covered by a dialog or clipped out of a carousel.
        const hit = document.elementFromPoint(Math.min(Math.max(c.x, 1), vw - 1), c.y);
        if (!hit || !(el.contains(hit) || hit.contains(el))) return;
      }
      out.push({ el, r });
    };
    for (const el of document.querySelectorAll(SEL)) add(el);
    if (document.body) {
      let n = 0;
      for (const el of document.body.getElementsByTagName('*')) {
        if (++n > 8000) break;
        if (seen.has(el)) continue;
        if (getComputedStyle(el).cursor !== 'pointer') continue;
        const p = el.parentElement;
        if (p && getComputedStyle(p).cursor === 'pointer') continue;
        add(el);
      }
    }
    root.classList.add('__couch_nomouse');
    return out;
  }

  function showRing() {
    // Pages can opt out (the Lounge home screen draws its own tvOS-style focus).
    if (C.cur && C.cur.closest && C.cur.closest('[data-couch-noring]')) return hideRing();
    C.ringOn = true;
    ring.style.display = 'block';
    if (!C.raf) tick();
  }
  function hideRing() {
    C.ringOn = false;
    if (ring) ring.style.display = 'none';
  }
  let ringChecks = 0;
  function tick() {
    C.raf = requestAnimationFrame(() => {
      C.raf = 0;
      if (!C.ringOn) return;
      const el = C.cur;
      if (!el || !el.isConnected) return hideRing();
      // Every ~quarter second: drop the ring once the show is playing again, or when the
      // focused control has faded out with the rest of the player UI.
      if (++ringChecks % 15 === 0) {
        const v = video();
        if (v && !v.paused && Date.now() > C.navUntil) return hideRing();
        if (el.checkVisibility && !el.checkVisibility({ opacityProperty: true, visibilityProperty: true })) return hideRing();
      }
      dom();
      const r = el.getBoundingClientRect(), pad = 6;
      ring.style.left = r.left - pad + 'px';
      ring.style.top = r.top - pad + 'px';
      ring.style.width = r.width + pad * 2 + 'px';
      ring.style.height = r.height + pad * 2 + 'px';
      tick();
    });
  }

  function focus(el, withVideo) {
    C.cur = el;
    let r = el.getBoundingClientRect();
    if (r.top < 0 || r.bottom > innerHeight || r.left < 0 || r.right > innerWidth) {
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      r = el.getBoundingClientRect();
    }
    try {
      if (el.tabIndex >= 0) el.focus({ preventScroll: true });
    } catch {}
    showRing();
    const c = center(r);
    // Over a video, hovering keeps the player controls on screen. Elsewhere hovering
    // pops site previews over neighbours, so park the pointer out of the way instead.
    if (withVideo) {
      C.quietUntil = Date.now() + 200;
      return { act: 'hover', x: c.x, y: c.y };
    }
    C.quietUntil = Date.now() + 200;
    return { act: 'park' };
  }

  function first(cands, withVideo) {
    const ax = withVideo ? innerWidth / 2 : 0, ay = withVideo ? innerHeight * 0.92 : 0;
    let best = null, bs = Infinity;
    for (const { el, r } of cands) {
      if (r.bottom <= 0 || r.top >= innerHeight) continue;
      const c = center(r), s = Math.hypot(c.x - ax, (c.y - ay) * 1.5);
      if (s < bs) (bs = s), (best = el);
    }
    return best ? focus(best, withVideo) : { act: 'none' };
  }

  function move(dir, withVideo) {
    const cands = candidates();
    let cur = C.cur && C.cur.isConnected ? C.cur : null;
    if (cur) {
      const r = cur.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) cur = null;
    }
    if (!cur) return first(cands, withVideo);
    const a = cur.getBoundingClientRect(), ac = center(a);
    let best = null, bs = Infinity;
    for (const { el, r } of cands) {
      if (el === cur || el.contains(cur) || cur.contains(el)) continue;
      const c = center(r);
      let p, o;
      if (dir === 'right' || dir === 'left') {
        if (dir === 'right' ? c.x <= ac.x + 2 || r.right <= a.right - 1 : c.x >= ac.x - 2 || r.left >= a.left + 1) continue;
        p = Math.max(0, dir === 'right' ? r.left - a.right : a.left - r.right);
        o = overlaps(a.top, a.bottom, r.top, r.bottom) ? 0 : Math.abs(c.y - ac.y);
      } else {
        if (dir === 'down' ? c.y <= ac.y + 2 || r.bottom <= a.bottom - 1 : c.y >= ac.y - 2 || r.top >= a.top + 1) continue;
        p = Math.max(0, dir === 'down' ? r.top - a.bottom : a.top - r.bottom);
        o = overlaps(a.left, a.right, r.left, r.right) ? Math.abs(c.x - ac.x) * 0.1 : Math.abs(c.x - ac.x);
      }
      const s = p + o * 2.5;
      if (s < bs) (bs = s), (best = el);
    }
    if (best) return focus(best, withVideo);
    if (dir === 'up' || dir === 'down') {
      // Nothing further that way on screen: scroll whatever is under the middle of the page.
      return { act: 'wheel', x: innerWidth / 2, y: innerHeight / 2, dy: (dir === 'down' ? 1 : -1) * innerHeight * 0.6 };
    }
    return { act: 'none' };
  }

  C.cmd = (k) => {
    dom();
    // A remote press while the screensaver is up only dismisses it.
    if (k !== 'init') {
      S.lastInput = Date.now();
      if (S.on) {
        S.hide();
        return { act: 'none', mode: 'nav', site: siteName(), input: false, vw: innerWidth, vh: innerHeight };
      }
    }
    const v = video();
    const playing = !!v && Date.now() > C.navUntil;
    const arrows = { up: 1, down: 1, left: 1, right: 1 };
    let res = { act: 'none' };
    if (k === 'init') {
      res = first(candidates(), false);
    } else if (arrows[k]) {
      if (playing && (k === 'left' || k === 'right')) {
        res = { act: 'key', key: k === 'left' ? 'ArrowLeft' : 'ArrowRight' };
      } else if (playing) {
        // Up/down while watching: bring up the player controls, then navigate them.
        C.navUntil = Date.now() + PLAYER_NAV_MS;
        C.cur = null;
        res = { act: 'wake' };
      } else {
        if (v) C.navUntil = Date.now() + PLAYER_NAV_MS;
        res = move(k, !!v);
      }
    } else if (k === 'ok') {
      if (playing) {
        if (document.activeElement && document.activeElement !== document.body) document.activeElement.blur();
        res = { act: 'key', key: 'Space' };
      } else {
        if (v) C.navUntil = Date.now() + PLAYER_NAV_MS;
        const el = C.cur && C.cur.isConnected ? C.cur : null;
        if (!el) res = first(candidates(), !!v);
        else {
          const c = center(el.getBoundingClientRect());
          // The server moves the pointer here to click; that's not the trackpad, so no cursor dot.
          C.quietUntil = Date.now() + 300;
          res = { act: 'click', x: c.x, y: c.y, input: isInput(el) };
        }
      }
    } else if (k === 'back') {
      if (v && !playing) {
        C.navUntil = 0;
        hideRing();
      } else if (document.fullscreenElement) {
        res = { act: 'key', key: 'Escape' };
      } else if (!document.documentElement.hasAttribute('data-couch-home')) {
        history.back();
      }
    } else if (k === 'playpause' || k === 'rew' || k === 'ffwd') {
      if (v) {
        C.navUntil = 0;
        if (document.activeElement && document.activeElement !== document.body) document.activeElement.blur();
        res = { act: 'key', key: k === 'playpause' ? 'Space' : k === 'rew' ? 'ArrowLeft' : 'ArrowRight' };
      }
    } else if (k === 'fullscreen') {
      res = { act: 'key', key: 'f' };
    }
    const mode = v && Date.now() > C.navUntil ? 'player' : 'nav';
    if (mode === 'player') hideRing();
    res.mode = mode;
    res.site = siteName();
    res.input = res.input || isInput(document.activeElement);
    res.vw = innerWidth;
    res.vh = innerHeight;
    return res;
  };

  // ---------- Screensaver ----------
  // Shown after 3 idle minutes on the home screen, or when the show has been paused for
  // 10 idle minutes. Any input only dismisses it: that event (and its keyup/click) is eaten.
  // Landscape photos slowly pan and zoom and crossfade about once a minute, with a big clock
  // in the lower left. Streaming sites can't load our local photos (https pages may not reach
  // the loopback address), so there it shows the show's poster, or a dark drifting gradient.
  // Built with inline styles and the Web Animations API so strict site CSPs can't block it.
  const HOME_IDLE = 3 * 60e3, PAUSE_IDLE = 10 * 60e3, PHOTO_MS = 60e3, FADE_MS = 3000, SHIFT_EVERY = 180;
  const S = (C.saver = { on: false, lastInput: Date.now(), pausedAt: 0, shownAt: 0, mx: -1, my: -1, eat: null, eatUntil: 0 });
  const timeFmt = new Intl.DateTimeFormat([], { hour: 'numeric', minute: '2-digit' });
  const hm = (d) => timeFmt.formatToParts(d).filter((p) => p.type !== 'dayPeriod').map((p) => p.value).join('').trim();
  let sv, svGrad, svLayers, svFront = 0, svBox, svDate, svTime, svMark, svAnims = [], svKb = [null, null];
  let svTimer = 0, svPhotoTimer = 0, svTicks = 0, svVideo = false, svList = null, svCur = '', svGen = 0;

  function saverBuild() {
    const el = (css, parent, tag) => {
      const d = document.createElement(tag || 'div');
      d.style.cssText = css;
      if (parent) parent.appendChild(d);
      return d;
    };
    sv = el('position:fixed;inset:0;z-index:2147483646;overflow:hidden;margin:0;padding:0;border:0;' +
      'background:#0a0b0d;opacity:0;transition:opacity 1.2s ease;cursor:none;pointer-events:auto;contain:strict;');
    sv.id = '__couch_saver';
    // Fallback: dark slate with slow teal, ink-blue and low amber light. Nothing purple.
    svGrad = el('position:absolute;inset:0;display:none;background:linear-gradient(160deg,#0d1418,#090b0e 55%,#0e0c09);', sv);
    // [color, center x (vw), center y (vh), diameter (vw), drift dx (vw), dy (vh), scale, period ms]
    const blobs = [
      ['rgba(22,78,92,.75)', 22, 10, 90, 14, 10, 1.15, 53e3],
      ['rgba(16,40,72,.8)', 86, 18, 80, -12, 12, 1.2, 61e3],
      ['rgba(20,96,104,.45)', 70, 92, 70, -16, -8, 1.12, 47e3],
      ['rgba(150,88,34,.42)', 14, 108, 70, 16, -10, 1.2, 67e3],
      ['rgba(36,52,62,.7)', 50, 60, 60, -10, -6, 1.25, 71e3],
    ];
    for (const [c, x, y, size, dx, dy, sc, ms] of blobs) {
      const b = el(`position:absolute;left:${x}vw;top:${y}vh;width:${size}vw;height:${size}vw;` +
        `margin:-${size / 2}vw 0 0 -${size / 2}vw;border-radius:50%;` +
        `background:radial-gradient(closest-side,${c},${c.replace(/[\d.]+\)$/, (m) => +m.slice(0, -1) * 0.4 + ')')} 50%,rgba(0,0,0,0));` +
        'will-change:transform;', svGrad);
      b.dataset.k = JSON.stringify([dx, dy, sc, ms]);
    }
    // Two photo layers that crossfade.
    svLayers = [0, 1].map(() => {
      const img = el('position:absolute;inset:0;width:100%;height:100%;max-width:none;margin:0;padding:0;border:0;' +
        `object-fit:cover;opacity:0;transition:opacity ${FADE_MS}ms ease;will-change:transform,opacity;`, sv, 'img');
      img.alt = '';
      img.decoding = 'async';
      img.referrerPolicy = 'no-referrer';
      return img;
    });
    // A touch of dimming for a dark room, a soft vignette, and a darker lower left for the clock.
    el('position:absolute;inset:0;background:rgba(6,8,10,.12);', sv);
    el('position:absolute;inset:0;background:radial-gradient(ellipse 75% 70% at 55% 45%,rgba(0,0,0,0) 45%,rgba(0,0,0,.5) 100%);', sv);
    el('position:absolute;inset:0;background:linear-gradient(18deg,rgba(0,0,0,.62) 0%,rgba(0,0,0,.34) 26%,rgba(0,0,0,0) 52%);', sv);
    el('position:absolute;inset:0;background:linear-gradient(0deg,rgba(0,0,0,.35) 0%,rgba(0,0,0,0) 30%);', sv);
    const shadow = 'text-shadow:0 .2vh 1.5vh rgba(0,0,0,.35),0 0 .3vh rgba(0,0,0,.15);';
    svBox = el('position:absolute;left:6.5vw;bottom:10vh;color:#fff;transition:transform 10s cubic-bezier(.4,0,.2,1);' +
      'will-change:transform;-webkit-font-smoothing:antialiased;pointer-events:none;text-align:left;', sv);
    svDate = el('margin:0 0 0 .5vh;font:500 3.5vh/1.2 "Inter","Inter Display",system-ui,sans-serif;letter-spacing:-.01em;' +
      'color:rgba(255,255,255,.9);' + shadow, svBox);
    svTime = el('margin:-.6vh 0 0;font:200 19vh/1 "Inter Display","Inter",system-ui,sans-serif;letter-spacing:-.035em;' +
      'font-variant-numeric:tabular-nums;font-feature-settings:"tnum" 1;color:rgba(255,255,255,.96);' + shadow, svBox);
    svMark = el('position:absolute;right:4.5vw;bottom:6.5vh;padding:0 0 .9vh 3vw;border-bottom:1px solid rgba(255,255,255,.4);' +
      'font:600 2.6vh/1 "Inter Display","Inter",system-ui,sans-serif;letter-spacing:-.025em;color:rgba(255,255,255,.88);' +
      'transition:transform 10s cubic-bezier(.4,0,.2,1);pointer-events:none;' + shadow, sv);
    svMark.textContent = 'Lounge';
  }

  // Slow Ken Burns move: zoom in a little while drifting in a random direction.
  function kenBurns(i, ms, loop) {
    if (svKb[i]) svKb[i].cancel();
    const r = (n) => (Math.random() * 2 - 1) * n;
    const s0 = 1.03 + Math.random() * 0.04, s1 = 1.14 + Math.random() * 0.06;
    const from = `scale(${s0.toFixed(3)}) translate(${r(1.5).toFixed(2)}%,${r(1).toFixed(2)}%)`;
    const to = `scale(${s1.toFixed(3)}) translate(${r(3.5).toFixed(2)}%,${r(2.5).toFixed(2)}%)`;
    svKb[i] = svLayers[i].animate([{ transform: from }, { transform: to }],
      { duration: ms, easing: 'ease-in-out', fill: 'both', ...(loop ? { direction: 'alternate', iterations: Infinity } : {}) });
  }

  // Load src into the hidden layer, then crossfade to it. failed() if it can't load.
  function showPhoto(src, loop, failed) {
    const gen = svGen, back = 1 - svFront, img = svLayers[back];
    img.onload = () => {
      if (gen !== svGen || !S.on) return;
      kenBurns(back, loop ? 50e3 : PHOTO_MS + FADE_MS * 2 + 5e3, loop);
      img.style.opacity = '1';
      svLayers[svFront].style.opacity = '0';
      svFront = back;
      svCur = src;
      svGrad.style.display = 'none';
      for (const a of svAnims) a.cancel();
      svAnims = [];
    };
    img.onerror = () => {
      if (gen === svGen && S.on && failed) failed();
    };
    img.src = src;
  }

  function nextPhoto() {
    if (!S.on || !svList || !svList.length) return;
    const pool = svList.length > 1 ? svList.filter((u) => u !== svCur) : svList;
    const src = pool[Math.floor(Math.random() * pool.length)];
    if (src === svCur) return;
    // A bad photo: drop it and try another; with none left, the gradient stays.
    showPhoto(src, false, () => {
      svList = svList.filter((u) => u !== src);
      if (!svCur) nextPhoto();
    });
  }

  function useGradient() {
    svGrad.style.display = 'block';
    if (svAnims.length) return;
    for (const b of svGrad.querySelectorAll('[data-k]')) {
      const [dx, dy, sc, ms] = JSON.parse(b.dataset.k);
      svAnims.push(b.animate([{ transform: 'translate(0,0) scale(1)' }, { transform: `translate(${dx}vw,${dy}vh) scale(${sc})` }],
        { duration: ms, direction: 'alternate', iterations: Infinity, easing: 'ease-in-out' }));
    }
  }

  // The paused show's own artwork, if the page has some we can load over https.
  function poster() {
    try {
      const m = navigator.mediaSession && navigator.mediaSession.metadata;
      const art = m && m.artwork && m.artwork.length ? m.artwork[m.artwork.length - 1].src : '';
      const og = document.querySelector('meta[property="og:image"],meta[name="og:image"],meta[name="twitter:image"]');
      for (const u of [art, og && og.content]) {
        if (!u) continue;
        const abs = new URL(u, location.href);
        if (abs.protocol === 'https:' || abs.origin === location.origin) return abs.href;
      }
    } catch {}
    return '';
  }

  function saverTick() {
    const d = new Date(), t = hm(d), ds = d.toLocaleDateString([], { weekday: 'long', month: 'long', day: 'numeric' });
    if (svTime.textContent !== t) svTime.textContent = t;
    if (svDate.textContent !== ds) svDate.textContent = ds;
    // Burn-in: nudge the clock and the mark a few dozen pixels every few minutes.
    if (++svTicks % SHIFT_EVERY === 0) {
      const r = (n) => ((Math.random() * 2 - 1) * n).toFixed(0);
      svBox.style.transform = `translate(${r(40)}px,${r(30)}px)`;
      svMark.style.transform = `translate(${r(20)}px,${r(16)}px)`;
    }
    // The show started again (autoplay, another device): get out of the way.
    if (svVideo) {
      const v = video();
      if (v && !v.paused) S.hide();
    }
  }

  S.show = (forVideo) => {
    if (S.on || !document.body) return;
    if (!sv) saverBuild();
    const fs = document.fullscreenElement;
    (fs && fs.tagName !== 'VIDEO' ? fs : document.body).appendChild(sv);
    S.on = true;
    S.shownAt = Date.now();
    const gen = ++svGen;
    svVideo = !!forVideo;
    svTicks = 0;
    svCur = '';
    for (const e of [svBox, svMark]) {
      e.style.transition = 'none';
      e.style.transform = 'none';
    }
    for (const img of svLayers) {
      img.style.transition = 'none';
      img.style.opacity = '0';
    }
    void sv.offsetWidth;
    for (const e of [svBox, svMark]) e.style.transition = 'transform 10s cubic-bezier(.4,0,.2,1)';
    for (const img of svLayers) img.style.transition = `opacity ${FADE_MS}ms ease`;
    sv.style.transition = 'opacity 1.2s ease';
    sv.style.opacity = '1';
    svGrad.style.display = 'none';
    if (isHome()) {
      // Our own page: the photo set from the server.
      const start = (list) => {
        if (gen !== svGen) return;
        svList = list && list.length ? list : null;
        if (!svList) return useGradient();
        nextPhoto();
        clearInterval(svPhotoTimer);
        svPhotoTimer = setInterval(nextPhoto, PHOTO_MS);
      };
      if (svList) start(svList);
      else fetch('/api/saver').then((r) => r.json()).then(start, () => start(null));
    } else {
      // A streaming site: the show's poster, else the gradient.
      const p = poster();
      if (p) {
        showPhoto(p, true, useGradient);
        // Keep the gradient moving underneath until the poster arrives.
        useGradient();
      } else useGradient();
    }
    hideRing();
    if (dot) dot.style.opacity = '0';
    saverTick();
    clearInterval(svTimer);
    svTimer = setInterval(saverTick, 1000);
  };

  S.hide = () => {
    if (!S.on) return;
    S.on = false;
    svGen++;
    clearInterval(svTimer);
    clearInterval(svPhotoTimer);
    sv.style.transition = 'opacity .35s ease';
    sv.style.opacity = '0';
    setTimeout(() => {
      if (S.on) return;
      for (const a of svAnims) a.cancel();
      svAnims = [];
      svKb.forEach((a, i) => a && (a.cancel(), (svKb[i] = null)));
      sv.remove();
    }, 400);
  };

  // What else to eat right after the event that dismissed the saver.
  const MOUSE_FOLLOW = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'dblclick', 'auxclick', 'contextmenu'];
  const FOLLOW = {
    keydown: [['keypress', 'keyup'], 1000],
    pointerdown: [MOUSE_FOLLOW.slice(1), 1000],
    mousedown: [MOUSE_FOLLOW.slice(2), 1000],
    wheel: [['wheel'], 600],
    // The server moves the pointer just before it clicks, so eat that click too.
    mousemove: [MOUSE_FOLLOW, 350],
  };
  const eat = (e) => {
    e.preventDefault();
    e.stopImmediatePropagation();
  };
  function onInput(e) {
    const t = e.type, now = Date.now();
    if (t === 'mousemove') {
      // Ignore synthetic moves that don't actually move (layout changes, our own overlay).
      if (Math.abs(e.clientX - S.mx) < 3 && Math.abs(e.clientY - S.my) < 3) return;
      S.mx = e.clientX;
      S.my = e.clientY;
    }
    if (S.eat && now < S.eatUntil && S.eat.includes(t)) return eat(e);
    if (!FOLLOW[t]) return;
    S.eat = null;
    S.lastInput = now;
    if (!S.on || (t === 'mousemove' && now - S.shownAt < 1500)) return;
    S.hide();
    [S.eat, S.eatUntil] = [FOLLOW[t][0], now + FOLLOW[t][1]];
    eat(e);
  }
  for (const t of ['keydown', 'keypress', 'keyup', 'mousemove', ...MOUSE_FOLLOW])
    addEventListener(t, onInput, { capture: true });
  addEventListener('wheel', onInput, { capture: true, passive: false });

  // Cheap idle check.
  setInterval(() => {
    if (S.on || document.hidden) return;
    const now = Date.now();
    if (isHome()) {
      if (now - S.lastInput >= HOME_IDLE) S.show(false);
      return;
    }
    const v = video();
    if (!v || !v.paused) return void (S.pausedAt = 0);
    if (!S.pausedAt) S.pausedAt = now;
    if (now - Math.max(S.pausedAt, S.lastInput) >= PAUSE_IDLE) S.show(true);
  }, 15000);

  // Big on-screen message, e.g. "Press power again to turn off".
  let toastEl, toastTimer;
  C.toast = (msg) => {
    if (!document.body) return;
    if (!toastEl) {
      toastEl = document.createElement('div');
      toastEl.style.cssText =
        'position:fixed;left:50%;top:6vh;transform:translateX(-50%);z-index:2147483647;pointer-events:none;' +
        'padding:16px 32px;border-radius:999px;background:rgba(36,36,40,.72);color:#fff;' +
        '-webkit-backdrop-filter:blur(30px) saturate(180%);backdrop-filter:blur(30px) saturate(180%);' +
        'font:500 25px/1.2 "Inter",system-ui,sans-serif;letter-spacing:-.01em;' +
        'box-shadow:0 12px 40px rgba(0,0,0,.45),inset 0 0 0 1px rgba(255,255,255,.12);' +
        'transition:opacity .25s;opacity:0';
    }
    const fs = document.fullscreenElement;
    (fs && fs.tagName !== 'VIDEO' ? fs : document.body).appendChild(toastEl);
    toastEl.textContent = msg;
    requestAnimationFrame(() => (toastEl.style.opacity = '1'));
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastEl.style.opacity = '0'), 3500);
  };

  // Show a cursor dot when the phone touchpad moves the (otherwise invisible) pointer.
  addEventListener(
    'mousemove',
    (e) => {
      if (Date.now() < C.quietUntil || !dom()) return;
      dot.style.display = 'block';
      dot.style.opacity = '1';
      dot.style.left = e.clientX + 'px';
      dot.style.top = e.clientY + 'px';
      hideRing();
      clearTimeout(dotTimer);
      dotTimer = setTimeout(() => (dot.style.opacity = '0'), 5000);
    },
    true,
  );
  addEventListener(
    'mousedown',
    () => {
      if (!dot || dot.style.display !== 'block') return;
      dot.classList.remove('click');
      void dot.offsetWidth;
      dot.classList.add('click');
    },
    true,
  );

  // Pressing Play in the player controls: get the ring out of the picture shortly after.
  addEventListener('playing', () => (C.navUntil = Math.min(C.navUntil, Date.now() + 1500)), true);

  // YouTube's "Auto" quality aims low for a TV across the room: ask for its best stream.
  if (location.hostname.endsWith('youtube.com')) {
    addEventListener(
      'playing',
      () => {
        const p = document.getElementById('movie_player');
        const best = p && p.getAvailableQualityLevels && p.getAvailableQualityLevels()[0];
        if (best && best !== 'auto' && p.getPlaybackQuality() !== best) p.setPlaybackQualityRange(best, best);
      },
      true,
    );
  }

  // A real keyboard plugged into the PC can drive the home screen too.
  addEventListener('keydown', (e) => {
    if (!document.documentElement.hasAttribute('data-couch-home')) return;
    const k = { ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right', Enter: 'ok' }[e.key];
    if (!k) return;
    e.preventDefault();
    const r = C.cmd(k);
    if (r.act === 'click' && C.cur) C.cur.click();
  });
})();
