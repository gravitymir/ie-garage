// Shared slide-out chat + event log, present on every page.
// A handle tab sticks out from the right edge; clicking it slides the panel
// in from right to left. The feed mixes free-form chat messages with workshop
// events (wheels arrived, car checked in, car finished, …).
//
// Public API (other scripts can push into the feed):
//   GarageChat.open() / .close() / .toggle()
//   GarageChat.message(text, author)         // post a chat line
//   GarageChat.event(text, kind)             // log an event; kind:
//                                            //   'wheels' | 'checkin' | 'done' | 'parts' | 'generic'
//
// Persistence is local to the browser (localStorage) and syncs across open
// tabs. A real cross-user shared log will need a server backend later.
(function () {
  if (window.__chatInstalled) return;
  window.__chatInstalled = true;

  const STORE_KEY = "garage-chat-feed";
  const MAX_ITEMS = 500;

  const EVENT_META = {
    wheels:  { icon: "🛞", label: "Wheels" },
    checkin: { icon: "🚗", label: "Check-in" },
    done:    { icon: "✅", label: "Done" },
    parts:   { icon: "📦", label: "Parts" },
    generic: { icon: "📣", label: "Event" },
  };

  // ---------- storage ----------

  function loadFeed() {
    try {
      const raw = localStorage.getItem(STORE_KEY);
      const arr = raw ? JSON.parse(raw) : null;
      if (Array.isArray(arr)) return arr;
    } catch (_) {}
    return seedFeed();
  }

  function saveFeed(feed) {
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify(feed.slice(-MAX_ITEMS)));
    } catch (_) {}
  }

  function seedFeed() {
    const now = Date.now();
    return [
      { type: "event", kind: "generic", text: "Local chat & event log started.", ts: now - 1000 * 60 * 60 },
      { type: "event", kind: "checkin", text: "11D11111 (Jaguar XJ) checked in.", ts: now - 1000 * 60 * 42 },
      { type: "msg", author: "Sean", text: "Anyone seen the 19mm socket?", ts: now - 1000 * 60 * 38 },
      { type: "event", kind: "wheels", text: "4× winter wheels arrived for 07G8765.", ts: now - 1000 * 60 * 25 },
      { type: "event", kind: "done", text: "10C1234 finished and handed back to customer.", ts: now - 1000 * 60 * 12 },
    ];
  }

  // ---------- helpers ----------

  function fmtTime(ts) {
    const d = new Date(ts);
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    const t = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    if (sameDay) return t;
    return d.toLocaleDateString([], { day: "2-digit", month: "2-digit" }) + " " + t;
  }

  function esc(s) {
    return String(s == null ? "" : s)
      .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  // ---------- styles ----------

  function injectStyles() {
    const css = `
      #garage-chat {
        position: fixed;
        top: 0; right: 0;
        height: 100%;
        width: 540px;
        max-width: 90vw;
        z-index: 9500;
        font-family: "JetBrains Mono", monospace;
        transform: translateX(100%);
        transition: transform 0.28s cubic-bezier(0.4, 0, 0.2, 1);
        display: flex;
      }
      #garage-chat.open { transform: translateX(0); }

      /* Handle that sticks out from the left side of the panel */
      #garage-chat .chat-handle {
        position: absolute;
        left: -40px; top: 50%;
        transform: translateY(-50%);
        width: 40px;
        padding: 16px 0;
        background: #3275ac;
        color: #fff;
        border: none;
        border-radius: 8px 0 0 8px;
        cursor: pointer;
        font-family: inherit;
        font-weight: 600;
        font-size: 13px;
        letter-spacing: 0.08em;
        writing-mode: vertical-rl;
        text-orientation: mixed;
        box-shadow: -2px 2px 10px rgba(0,0,0,0.2);
        display: flex; align-items: center; justify-content: center; gap: 8px;
      }
      #garage-chat .chat-handle:hover { background: #2a5f8c; }
      #garage-chat .chat-handle .badge {
        background: #c33; color: #fff;
        border-radius: 10px; padding: 1px 6px;
        font-size: 11px; writing-mode: horizontal-tb;
        display: none;
      }
      #garage-chat .chat-handle .badge.show { display: inline-block; }

      .chat-panel {
        flex: 1;
        background: #fff;
        border-left: 1px solid #ccc;
        box-shadow: -4px 0 20px rgba(0,0,0,0.15);
        display: flex; flex-direction: column;
        min-width: 0;
      }
      .chat-head {
        display: flex; align-items: center; justify-content: space-between;
        gap: 8px; padding: 12px 14px;
        background: #3275ac; color: #fff;
      }
      .chat-head h3 { margin: 0; font-size: 1rem; font-weight: 600; }
      .chat-head .chat-close {
        background: rgba(255,255,255,0.15); color: #fff;
        border: none; width: 28px; height: 28px; border-radius: 14px;
        cursor: pointer; font-size: 16px; font-family: inherit; line-height: 1;
      }
      .chat-head .chat-close:hover { background: rgba(255,255,255,0.3); }

      .chat-feed {
        flex: 1; overflow-y: auto;
        padding: 12px; background: #f5f5f5;
        display: flex; flex-direction: column; gap: 8px;
      }
      .chat-empty { color: #888; font-size: 13px; text-align: center; margin-top: 20px; }

      .chat-item { font-size: 13px; line-height: 1.4; }
      .chat-item .when { color: #999; font-size: 11px; }

      .chat-item.msg .bubble {
        background: #fff; border: 1px solid #ddd; border-radius: 8px;
        padding: 7px 10px;
      }
      .chat-item.msg .who { color: #3275ac; font-weight: 600; margin-right: 6px; }
      .chat-item.msg .text { white-space: pre-wrap; word-break: break-word; }

      .chat-item.event .bubble {
        background: #fff7e6; border: 1px solid #f0d9a8;
        border-left: 4px solid #e6a700; border-radius: 6px;
        padding: 7px 10px;
        display: flex; gap: 8px; align-items: flex-start;
      }
      .chat-item.event .ico { font-size: 15px; line-height: 1.3; }
      .chat-item.event .ev-text { flex: 1; word-break: break-word; }
      .chat-item.event .ev-kind {
        color: #a07400; font-weight: 600; font-size: 11px;
        text-transform: uppercase; letter-spacing: 0.04em;
      }

      .chat-compose {
        border-top: 1px solid #ddd; padding: 10px; background: #fff;
        display: flex; flex-direction: column; gap: 6px;
      }
      .chat-compose input[type="text"], .chat-compose textarea {
        font-family: inherit; font-size: 13px;
        border: 1px solid #bbb; border-radius: 4px; padding: 6px 8px; width: 100%;
      }
      .chat-compose textarea { resize: none; min-height: 38px; max-height: 120px; }
      .chat-compose .row { display: flex; gap: 6px; }
      .chat-compose button {
        font-family: inherit; font-size: 13px; font-weight: 600;
        border: 1px solid #3275ac; background: #3275ac; color: #fff;
        border-radius: 4px; padding: 6px 14px; cursor: pointer;
      }
      .chat-compose button:hover { background: #2a5f8c; }
      .chat-compose .ev-btns { display: flex; gap: 4px; flex-wrap: wrap; }
      .chat-compose.locked { display: none; }
      .chat-locked {
        display: none;
        padding: 14px 12px;
        margin: 0 8px 8px;
        text-align: center;
        font-size: 12px;
        color: #888;
        background: #fafafa;
        border: 1px dashed #ccc;
        border-radius: 6px;
      }
      .chat-locked.visible { display: block; }
      .chat-compose .ev-btns button {
        background: #fff; color: #a07400; border-color: #f0d9a8;
        font-size: 11px; padding: 3px 8px;
      }
      .chat-compose .ev-btns button:hover { background: #fff7e6; }

      @media (max-width: 500px) {
        #garage-chat { width: 480px; }
      }
    `;
    const s = document.createElement("style");
    s.textContent = css;
    document.head.appendChild(s);
  }

  // ---------- markup ----------

  let feed = [];
  let isOpen = false;

  function injectMarkup() {
    const wrap = document.createElement("div");
    wrap.id = "garage-chat";
    wrap.innerHTML = `
      <button class="chat-handle" id="chat-handle" aria-label="Open chat">
        <span>CHAT</span>
        <span class="badge" id="chat-badge">0</span>
      </button>
      <div class="chat-panel">
        <div class="chat-head">
          <h3>Chat &amp; Events</h3>
          <button class="chat-close" id="chat-close" aria-label="Close">×</button>
        </div>
        <div class="chat-feed" id="chat-feed"></div>
        <div class="chat-compose">
          <div class="ev-btns" id="chat-ev-btns">
            <button data-kind="checkin">🚗 Check-in</button>
            <button data-kind="wheels">🛞 Wheels in</button>
            <button data-kind="parts">📦 Parts in</button>
            <button data-kind="done">✅ Done</button>
          </div>
          <div class="row">
            <textarea id="chat-input" placeholder="Write a message…" rows="1"></textarea>
            <button id="chat-send">Send</button>
          </div>
        </div>
        <div class="chat-locked" id="chat-locked">Sign in to write messages or log events.</div>
      </div>
    `;
    document.body.appendChild(wrap);
  }

  // ---------- rendering ----------

  function render() {
    const list = document.getElementById("chat-feed");
    if (!list) return;
    if (!feed.length) {
      list.innerHTML = `<div class="chat-empty">No messages yet.</div>`;
      return;
    }
    list.innerHTML = feed.map(renderItem).join("");
    list.scrollTop = list.scrollHeight;
  }

  function renderItem(it) {
    const when = `<span class="when">${fmtTime(it.ts)}</span>`;
    if (it.type === "event") {
      const meta = EVENT_META[it.kind] || EVENT_META.generic;
      return `
        <div class="chat-item event">
          <div class="bubble">
            <span class="ico">${meta.icon}</span>
            <div class="ev-text">
              <div><span class="ev-kind">${esc(meta.label)}</span> ${when}</div>
              <div>${esc(it.text)}</div>
            </div>
          </div>
        </div>`;
    }
    return `
      <div class="chat-item msg">
        <div class="bubble">
          <span class="who">${esc(it.author || "Anon")}</span>${when}
          <div class="text">${esc(it.text)}</div>
        </div>
      </div>`;
  }

  function updateBadge() {
    const badge = document.getElementById("chat-badge");
    if (!badge) return;
    // Show a dot/count of events+messages only while the panel is closed.
    if (isOpen) { badge.classList.remove("show"); return; }
  }

  // ---------- actions ----------

  function push(item) {
    feed.push(item);
    saveFeed(feed);
    render();
  }

  function authorName() {
    const w = typeof window.currentWorker === "function" ? window.currentWorker() : null;
    if (!w) return "";
    return `${w.first_name || ""} ${w.last_name || ""}`.trim() || w.id || "";
  }

  function isAuthed() {
    return !!authorName();
  }

  function refreshComposeAccess() {
    const compose = document.querySelector("#garage-chat .chat-compose");
    const locked = document.getElementById("chat-locked");
    if (!compose || !locked) return;
    const ok = isAuthed();
    compose.classList.toggle("locked", !ok);
    locked.classList.toggle("visible", !ok);
  }

  function sendMessage() {
    const input = document.getElementById("chat-input");
    if (!input) return;
    const author = authorName();
    if (!author) {
      refreshComposeAccess();
      return;
    }
    const text = input.value.trim();
    if (!text) return;
    push({ type: "msg", author, text, ts: Date.now() });
    input.value = "";
    input.style.height = "auto";
    input.focus();
  }

  function open() {
    const el = document.getElementById("garage-chat");
    if (el) el.classList.add("open");
    isOpen = true;
    updateBadge();
    render();
    const input = document.getElementById("chat-input");
    if (input) setTimeout(() => input.focus(), 150);
  }

  function close() {
    const el = document.getElementById("garage-chat");
    if (el) el.classList.remove("open");
    isOpen = false;
  }

  function toggle() { isOpen ? close() : open(); }

  // ---------- wiring ----------

  function wire() {
    document.getElementById("chat-handle").addEventListener("click", toggle);
    document.getElementById("chat-close").addEventListener("click", close);
    document.getElementById("chat-send").addEventListener("click", sendMessage);
    window.addEventListener("worker:login", refreshComposeAccess);
    window.addEventListener("worker:logout", refreshComposeAccess);
    window.addEventListener("focus", refreshComposeAccess);

    const input = document.getElementById("chat-input");
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendMessage(); }
    });
    // Auto-grow the textarea
    input.addEventListener("input", () => {
      input.style.height = "auto";
      input.style.height = Math.min(input.scrollHeight, 120) + "px";
    });

    document.getElementById("chat-ev-btns").addEventListener("click", (e) => {
      const btn = e.target.closest("button[data-kind]");
      if (!btn) return;
      if (!isAuthed()) { refreshComposeAccess(); return; }
      const kind = btn.dataset.kind;
      const what = prompt(`Log "${(EVENT_META[kind] || EVENT_META.generic).label}" event — describe it:`);
      if (what && what.trim()) {
        push({ type: "event", kind, text: what.trim(), ts: Date.now() });
      }
    });

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && isOpen) close();
    });

    // Sync feed across tabs/pages of the same browser.
    window.addEventListener("storage", (e) => {
      if (e.key === STORE_KEY) { feed = loadFeed(); render(); }
    });
  }

  function install() {
    if (document.getElementById("garage-chat")) return;
    if (!document.body) return;
    injectStyles();
    injectMarkup();
    feed = loadFeed();
    wire();
    render();
    refreshComposeAccess();
  }

  // ---------- public API ----------

  window.GarageChat = {
    open, close, toggle,
    message(text, author) {
      if (!text) return;
      push({ type: "msg", author: author || "System", text: String(text), ts: Date.now() });
    },
    event(text, kind) {
      if (!text) return;
      const k = EVENT_META[kind] ? kind : "generic";
      push({ type: "event", kind: k, text: String(text), ts: Date.now() });
    },
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", install);
  } else {
    install();
  }
})();
