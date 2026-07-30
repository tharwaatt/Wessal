// content.js — Wessal Bridge v7.0 — Local IDE Connector
//
// Architecture: ZERO DOM scraping. The OS clipboard is the ONLY source of truth.
//
// Workflow:
//   1. AI writes response containing [WESSAL:...] tags.
//   2. User clicks the platform's native "Copy Message" button (copies raw markdown).
//   3. User clicks the ⚡ button.
//   4. Extension reads clipboard, validates tags, sends to Rust daemon.
//   5. Rust executes actions, sends result back via WebSocket.
//   6. Extension pastes result into the chat input.

(function WessalBridge() {
  'use strict';

  // ── State ─────────────────────────────────────────────────────────────────
  let ws                    = null;
  let retryMs               = 500;
  let pendingCollectPayload = null;
  let buttonIsRunning       = false;
  let runningTimeout        = null;

  const BTN_ID = 'wessal-collect-btn';

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: TAG DETECTION
  // ═══════════════════════════════════════════════════════════════════════════

  function hasActionTags(text) {
    return (text || '').includes('[WESSAL:') || (text || '').includes('[DEVMATE:');
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: CHAT INPUT (for pasting results back)
  // ═══════════════════════════════════════════════════════════════════════════

  function isElementVisible(el) {
    if (!el || el.offsetWidth === 0 || el.offsetHeight === 0) return false;
    const s = window.getComputedStyle(el);
    return s.display !== 'none' && s.visibility !== 'hidden' && parseFloat(s.opacity ?? '1') > 0;
  }

  // Heuristic: score by (area × 0.4) + (bottom-proximity × 60).
  // Chat inputs are large and sit near the bottom of the viewport on every platform.
  function getChatInput() {
    const candidates = [
      ...document.querySelectorAll('[contenteditable="true"]'),
      ...document.querySelectorAll('textarea'),
    ].filter(el => {
      if (!isElementVisible(el)) return false;
      if (el.id === BTN_ID || el.closest(`#${BTN_ID}`)) return false;
      const r = el.getBoundingClientRect();
      return r.width > 50 && r.height > 20;
    });

    if (candidates.length === 0) return null;
    if (candidates.length === 1) return candidates[0];

    const vh = window.innerHeight;
    return candidates
      .map(el => {
        const r = el.getBoundingClientRect();
        return { el, score: r.width * r.height * 0.4 + (vh - r.top) * 60 };
      })
      .sort((a, b) => b.score - a.score)[0].el;
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: PASTE HELPERS
  // ═══════════════════════════════════════════════════════════════════════════

  function waitInput(ms) {
    return new Promise(resolve => {
      const el = getChatInput();
      if (el) { resolve(el); return; }
      const t0 = Date.now();
      const iv = setInterval(() => {
        const found = getChatInput();
        if (found || Date.now() - t0 > ms) { clearInterval(iv); resolve(found || null); }
      }, 50);
    });
  }

  async function pasteResult(text) {
    const el = getChatInput() || await waitInput(5_000);
    if (!el) { console.warn('[Wessal] no input element found'); return false; }

    const finalText = text.trimEnd() + '\n\n';
    el.focus();

    if (el.isContentEditable) {
      document.execCommand('selectAll', false, null);
      document.execCommand('delete', false, null);
      const dt = new DataTransfer();
      dt.setData('text/plain', finalText);
      el.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: dt }));
      await new Promise(requestAnimationFrame);
      if (!el.innerText || el.innerText.length < 5) {
        el.innerText = finalText;
        el.dispatchEvent(new Event('input', { bubbles: true }));
      }
      const range = document.createRange();
      range.selectNodeContents(el);
      range.collapse(false);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      return true;
    }

    if (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT') {
      const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
      setter.call(el, finalText);
      el.dispatchEvent(new Event('input',  { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      el.selectionStart = el.selectionEnd = el.value.length;
      return true;
    }

    return false;
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: WEBSOCKET
  // ═══════════════════════════════════════════════════════════════════════════

  function connect() {
    try { ws = new WebSocket('ws://localhost:7878'); } catch (_) { schedReconnect(); return; }

    ws.onopen = () => {
      retryMs = 500;
      console.info('[Wessal] WebSocket connected');
      wsSend({ type: 'ready', url: location.href });
      flushPendingCollect();
    };

    ws.onmessage = ev => {
      let m; try { m = JSON.parse(ev.data); } catch { return; }
      handleCmd(m);
    };

    ws.onclose = () => {
      ws = null;
      if (buttonIsRunning) {
        console.warn('[Wessal] WS closed mid-flight — resetting button to idle');
        setButtonState('idle');
      }
      schedReconnect();
    };

    ws.onerror = () => {};
  }

  function schedReconnect() {
    setTimeout(connect, retryMs);
    retryMs = Math.min(retryMs * 2, 16_000);
  }

  function wsSend(obj) {
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj));
    } else if (obj.type === 'collect_requested') {
      console.info('[Wessal] WS not ready — buffering collect');
      pendingCollectPayload = obj;
    }
  }

  function flushPendingCollect() {
    if (pendingCollectPayload && ws?.readyState === WebSocket.OPEN) {
      console.info('[Wessal] flushing buffered collect');
      ws.send(JSON.stringify(pendingCollectPayload));
      pendingCollectPayload = null;
      setButtonState('running');
    }
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: COLLECT — Pure Clipboard
  // ═══════════════════════════════════════════════════════════════════════════

  async function collect() {
    // Step 1: Read clipboard
    let clipboardText = '';
    try {
      clipboardText = await navigator.clipboard.readText();
    } catch (e) {
      console.warn('[Wessal] Clipboard read failed/denied:', e.message);
      setButtonState('error');
      return;
    }

    // Step 2: Validate — must contain Wessal tags
    if (!hasActionTags(clipboardText)) {
      console.warn('[Wessal] No WESSAL tags found in clipboard. Did you copy the AI message first?');
      setButtonState('error');
      return;
    }

    // Step 3: Send to Rust
    const tagCount = (clipboardText.match(/\[(?:WESSAL|DEVMATE):/gi) || []).length;
    console.info(`[Wessal] collect — ${tagCount} tag(s), ${clipboardText.length} chars`);

    const payload = { type: 'collect_requested', full_text: clipboardText, code_blocks: [] };

    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(payload));
    } else {
      pendingCollectPayload = payload;
      setButtonState('queued');
    }
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: BUTTON UI
  // ═══════════════════════════════════════════════════════════════════════════

  function getBtn() { return document.getElementById(BTN_ID); }

  function createCollectBtn() {
    if (getBtn()) return;

    const btn = document.createElement('button');
    btn.id    = BTN_ID;
    btn.title = 'Wessal — copy the AI message first, then click to execute tags';

    // `all: initial !important` strips any host-page wildcard overrides first;
    // every subsequent declaration is also !important so inline-style wins ties.
    btn.style.cssText = [
      'all:initial !important',
      'position:fixed !important',
      'top:auto !important', 'left:auto !important', 'bottom:90px !important', 'right:18px !important',
      'z-index:2147483647 !important',
      'display:flex !important', 'align-items:center !important', 'justify-content:center !important',
      'box-sizing:border-box !important', 'width:44px !important', 'height:44px !important',
      'margin:0 !important', 'padding:0 !important',
      'border-radius:50% !important', 'border:2px solid #4a9eff !important',
      'background:#12121e !important', 'color:#4a9eff !important',
      'font-size:20px !important', 'font-family:system-ui,-apple-system,sans-serif !important',
      'line-height:1 !important', 'cursor:pointer !important',
      'pointer-events:auto !important', 'opacity:1 !important', 'visibility:visible !important',
      'transform:none !important', 'filter:none !important',
      'mix-blend-mode:normal !important', 'clip-path:none !important', 'mask:none !important',
      'outline:none !important',
      'transition:background-color 0.2s,color 0.2s,transform 0.2s !important',
      'box-shadow:0 2px 12px rgba(74,158,255,0.25) !important',
    ].join(';');
    btn.textContent = '⚡';

    btn.onmouseover = () => {
      btn.style.setProperty('background', '#4a9eff',     'important');
      btn.style.setProperty('color',      '#fff',        'important');
      btn.style.setProperty('transform',  'scale(1.08)', 'important');
    };
    btn.onmouseout = () => {
      btn.style.setProperty('background', '#12121e',   'important');
      btn.style.setProperty('color',      '#4a9eff',   'important');
      btn.style.setProperty('transform',  'none',      'important');
    };
    btn.onclick = async () => { setButtonState('running'); await collect(); };

    // Append to <html>, not <body>. SPA page-transition animations often apply
    // CSS `transform` to <body>, which creates a new containing block and breaks
    // position:fixed viewport anchoring. <html> is never transformed.
    document.documentElement.appendChild(btn);
  }

  function setButtonState(state) {
    const btn = getBtn();
    if (!btn) return;
    clearTimeout(runningTimeout);
    runningTimeout = null;

    const STATES = {
      idle:    { icon: '⚡', border: '#4a9eff', color: '#4a9eff', pointer: '',     delay: 0    },
      running: { icon: '⏳', border: '#f59e0b', color: '#f59e0b', pointer: 'none', delay: 0    },
      queued:  { icon: '⌛', border: '#a855f7', color: '#a855f7', pointer: 'none', delay: 0    },
      done:    { icon: '✓',  border: '#22c55e', color: '#22c55e', pointer: '',     delay: 1500 },
      error:   { icon: '✗',  border: '#ef4444', color: '#ef4444', pointer: '',     delay: 2000 },
    };

    const cfg = STATES[state] || STATES.idle;
    btn.textContent = cfg.icon;

    // Must use setProperty(..., 'important') — the CSSOM camelCase setter always
    // writes priority '' which would silently drop the !important flag and expose
    // these declarations to host-page overrides.
    btn.style.setProperty('border-color',    cfg.border,            'important');
    btn.style.setProperty('color',           cfg.color,             'important');
    btn.style.setProperty('pointer-events',  cfg.pointer || 'auto', 'important');

    buttonIsRunning = (state === 'running' || state === 'queued');

    // Safety net: if Rust never responds (WS drop, parse error, etc.) the button
    // self-recovers to idle after 30 s instead of staying frozen indefinitely.
    if (buttonIsRunning) {
      runningTimeout = setTimeout(() => {
        console.warn('[Wessal] safety timeout fired — resetting to idle');
        setButtonState('idle');
      }, 30_000);
    }

    if (cfg.delay > 0) setTimeout(() => setButtonState('idle'), cfg.delay);
  }

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: BUTTON RE-INJECTION (SPA navigation guard)
  // ═══════════════════════════════════════════════════════════════════════════

  // Minimal observer: only re-injects the button if SPA navigation unmounts it.
  // No DOM reading, no streaming detection, no auto-execute.
  let debounce = null;
  new MutationObserver(() => {
    clearTimeout(debounce);
    debounce = setTimeout(() => {
      if (!getBtn()) createCollectBtn();
    }, 300);
  }).observe(document.body, { childList: true, subtree: true });

  // ═══════════════════════════════════════════════════════════════════════════
  // SECTION: COMMAND HANDLER (Rust → Extension)
  // ═══════════════════════════════════════════════════════════════════════════

  async function handleCmd(msg) {
    switch (msg.type) {
      case 'type_confirmation': {
        const ok = await pasteResult(msg.content);
        setButtonState(ok ? 'done' : 'error');
        wsSend({ type: 'paste_ack', ok });
        break;
      }
      case 'paste_context': {
        const ok = await pasteResult(msg.content);
        wsSend({ type: 'paste_ack', ok });
        break;
      }
    }
  }

  // ── Boot ──────────────────────────────────────────────────────────────────
  connect();
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', createCollectBtn);
  else createCollectBtn();
  console.info('[Wessal] v7.0 loaded on', location.hostname);

})();