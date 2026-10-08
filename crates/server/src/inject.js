// glyph page agent. Injected before any page script via Page.addScriptToEvaluateOnNewDocument.
// ~1 KB: kills animations/caret blink (we draw our own caret), and reports focus/caret/copy/dirty.
(() => {
  if (window.__glyphInit) return;
  window.__glyphInit = true;
  const send = (o) => { try { __glyph(JSON.stringify(o)); } catch (_) {} };

  // Page style. __TERMINAL__/__CW__/__CH__ are substituted by the server per tab.
  try {
    let css = '*,*::before,*::after{animation:none!important;transition:none!important;scroll-behavior:auto!important;caret-color:transparent!important}';
    if (__TERMINAL__) {
      // Lay the page out in a monospace font whose advance is exactly one cell, with one-cell line
      // height: every character lands in its own cell and consecutive lines never share a row.
      // The doubled :not(#_g) raises specificity so author rules (even !important ones) lose.
      const fam = 'ui-monospace,"SF Mono",SFMono-Regular,Menlo,Consolas,"DejaVu Sans Mono","Liberation Mono","Noto Sans Mono",monospace';
      const c2 = document.createElement('canvas').getContext('2d');
      c2.font = '100px ' + fam;
      const adv = c2.measureText('MMMMMMMMMM').width / 1000; // advance per px of font-size
      const size = adv > 0.2 ? Math.round((__CW__ / adv) * 100) / 100 : 13.33;
      const sel = '*:not(svg *):not(#_g):not(#_g)';
      css += sel + ',' + sel + '::before,' + sel + '::after,' + sel + '::placeholder{font-family:' + fam + '!important;font-size:' + size + 'px!important;line-height:__CH__px!important;letter-spacing:0!important;word-spacing:0!important;font-kerning:none!important;font-variant-ligatures:none!important}';
    }
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(css);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  } catch (_) {}

  const NON_TEXT = ['checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'image', 'color', 'range'];
  let timer = 0;
  const report = () => {
    timer = 0;
    const a = document.activeElement;
    if (a && ((a.tagName === 'INPUT' && !NON_TEXT.includes(a.type)) || a.tagName === 'TEXTAREA')) {
      const r = a.getBoundingClientRect();
      const pos = a.selectionStart == null ? (a.value || '').length : a.selectionStart;
      send({ t: 'caret', k: a.tagName === 'TEXTAREA' ? 'ta' : 'in', x: r.x, y: r.y, w: r.width, h: r.height,
             pre: (a.value || '').slice(Math.max(0, pos - 300), pos), pw: a.type === 'password' });
    } else if (a && a.isContentEditable) {
      const s = getSelection();
      let r = s && s.rangeCount ? s.getRangeAt(0).getBoundingClientRect() : a.getBoundingClientRect();
      if (!r || (r.x === 0 && r.y === 0 && r.width === 0)) r = a.getBoundingClientRect();
      send({ t: 'caret', k: 'ce', x: r.x, y: r.y, w: r.width, h: r.height, pre: '', pw: false });
    } else {
      send({ t: 'blur' });
    }
  };
  const soon = () => { if (!timer) timer = setTimeout(report, 25); };
  for (const ev of ['focusin', 'focusout', 'input', 'keyup', 'mouseup', 'click']) document.addEventListener(ev, soon, true);
  document.addEventListener('selectionchange', soon, true);

  document.addEventListener('copy', () => send({ t: 'copy', text: String(getSelection()) }), true);
  document.addEventListener('cut', () => send({ t: 'copy', text: String(getSelection()) }), true);
  try {
    const w = navigator.clipboard && navigator.clipboard.writeText;
    if (w) navigator.clipboard.writeText = function (t) { send({ t: 'copy', text: String(t) }); return Promise.resolve(); };
  } catch (_) {}

  // text mode: ping when the DOM changes (debounced); enabled by the server via __glyphWatch(true)
  let mo = null, dirty = 0;
  const ping = (ms) => { if (!dirty) dirty = setTimeout(() => { dirty = 0; send({ t: 'dirty' }); }, ms); };
  window.__glyphWatch = (on) => {
    if (mo) { mo.disconnect(); mo = null; }
    if (!on) return;
    mo = new MutationObserver(() => ping(400));
    mo.observe(document, { subtree: true, childList: true, characterData: true, attributes: true });
  };
  // typing changes input.value without any DOM mutation
  document.addEventListener('input', () => { if (mo) ping(120); }, true);

  // Scrolling happens here, on the main thread, instead of through compositor wheel events: the
  // DOM snapshot and the next screenshot then agree on the scroll offset. The container under
  // (x, y) scrolls if it can; otherwise the page does. Page scrolls snap to whole cell rows.
  const CELL_H = __CH__;
  const scrollable = (e, dy, dx) => {
    const s = getComputedStyle(e);
    if (dy) {
      if (!/(auto|scroll|overlay)/.test(s.overflowY) || e.scrollHeight <= e.clientHeight + 1) return false;
      if (dy > 0 ? e.scrollTop + e.clientHeight >= e.scrollHeight - 1 : e.scrollTop <= 0) return false;
      return true;
    }
    if (!/(auto|scroll|overlay)/.test(s.overflowX) || e.scrollWidth <= e.clientWidth + 1) return false;
    return dx > 0 ? e.scrollLeft + e.clientWidth < e.scrollWidth - 1 : e.scrollLeft > 0;
  };
  window.__glyphScroll = (x, y, dx, dy, edge) => {
    const root = document.scrollingElement || document.documentElement;
    if (edge) { root.scrollTo({ top: edge < 0 ? 0 : root.scrollHeight, left: root.scrollLeft, behavior: 'instant' }); return [root.scrollLeft, root.scrollTop]; }
    for (let el = document.elementFromPoint(x, y); el && el !== document.body && el !== document.documentElement; el = el.parentElement) {
      if (scrollable(el, dy, dx)) { el.scrollBy({ top: dy, left: dx, behavior: 'instant' }); return [root.scrollLeft, root.scrollTop]; }
    }
    root.scrollBy({ top: dy, left: dx, behavior: 'instant' });
    const snapped = Math.round(root.scrollTop / CELL_H) * CELL_H;
    if (dy && snapped !== root.scrollTop && snapped <= root.scrollHeight - root.clientHeight) root.scrollTo({ top: snapped, left: root.scrollLeft, behavior: 'instant' });
    return [root.scrollLeft, root.scrollTop];
  };
  // Resolves with the scroll offset once two animation frames have run, i.e. once the compositor
  // has committed the main thread's latest state.
  window.__glyphSettled = () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => {
    const root = document.scrollingElement || document.documentElement;
    r([root.scrollLeft, root.scrollTop]);
  })));
})();
