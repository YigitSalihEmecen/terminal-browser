// glyph page agent. Injected before any page script via Page.addScriptToEvaluateOnNewDocument.
// ~1 KB: kills animations/caret blink (we draw our own caret), and reports focus/caret/copy/dirty.
(() => {
  if (window.__glyphInit) return;
  window.__glyphInit = true;
  const send = (o) => { try { __glyph(JSON.stringify(o)); } catch (_) {} };

  try {
    const sheet = new CSSStyleSheet();
    sheet.replaceSync('*,*::before,*::after{animation:none!important;transition:none!important;scroll-behavior:auto!important;caret-color:transparent!important}');
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
  window.__glyphWatch = (on) => {
    if (mo) { mo.disconnect(); mo = null; }
    if (!on) return;
    mo = new MutationObserver(() => { if (!dirty) dirty = setTimeout(() => { dirty = 0; send({ t: 'dirty' }); }, 400); });
    mo.observe(document, { subtree: true, childList: true, characterData: true, attributes: true });
  };
})();
