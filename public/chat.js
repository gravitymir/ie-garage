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

  const EVENT_META = {
    wheels:  { icon: "🛞", label: "Wheels" },
    checkin: { icon: "🚗", label: "Check-in" },
    done:    { icon: "✅", label: "Done" },
    parts:   { icon: "📦", label: "Parts" },
    generic: { icon: "📣", label: "Event" },
  };

  // Polling cadence — two speeds:
  //   OPEN   → 20 s. User is looking at the feed, so fresh updates matter.
  //   CLOSED → 5 min. Just enough to notice something arrived and shake
  //            the handle; keeps request volume tiny while the chat sits
  //            in the corner.
  const POLL_OPEN_MS   = 20 * 1000;
  const POLL_CLOSED_MS = 5 * 60 * 1000;

  // ---------- server-side feed ----------
  //
  // The feed lives in cars/chat/messages.json on the server. Every browser
  // reads from the same file, so all workshop workstations see the same
  // conversation and event log. The client keeps an in-memory cache and
  // polls only what it doesn't already have via `?since=<maxTs>`.

  let feed = [];             // in-memory cache, newest last
  let maxTsSeen = 0;         // ts of the last message we already have
  let pollTimerId = null;    // active setInterval, whatever cadence
  let hasUnread = false;     // true when new stuff arrived while chat was closed

  // Stable identity for the presence counter. Prefer the signed-in
  // worker's id — multiple tabs from one Andrii deduplicate to one
  // "online" tick. If nobody is signed in (auth off, or on the login
  // screen), fall back to a per-tab random key from sessionStorage.
  function clientId() {
    const w = typeof window.currentWorker === "function" ? window.currentWorker() : null;
    if (w && w.id) return "w:" + w.id;
    let sid = null;
    try { sid = sessionStorage.getItem("gs_client_id"); } catch (_) {}
    if (!sid) {
      sid = "s:" + Math.random().toString(36).slice(2, 10)
                 + "-" + Date.now().toString(36);
      try { sessionStorage.setItem("gs_client_id", sid); } catch (_) {}
    }
    return sid;
  }

  function renderOnlineCount(n) {
    const nEl = document.querySelector("#chat-online .n");
    if (nEl) nEl.textContent = (typeof n === "number" && n > 0) ? String(n) : "–";
  }

  function feedUrl() {
    const qs = new URLSearchParams();
    if (maxTsSeen) qs.set("since", String(maxTsSeen));
    qs.set("client_id", clientId());
    return "/api/chat/feed?" + qs.toString();
  }

  async function fetchNewMessages() {
    try {
      const res = await fetch(feedUrl());
      if (!res.ok) return;
      const data = await res.json();
      renderOnlineCount(data.online);
      const items = Array.isArray(data.items) ? data.items : [];
      if (!items.length) return;
      for (const it of items) {
        feed.push(it);
        const t = Number(it.ts) || 0;
        if (t > maxTsSeen) maxTsSeen = t;
      }
      render();
      // New arrivals from the server = someone else's message or an event
      // fired on another machine. Ping the notification sound (gated by
      // the user's Settings → Chat toggle + volume).
      playNotificationSound();
      // Anything new that arrived while the chat is closed flips the
      // handle into its "look at me" state (colour flip + shake).
      if (!isOpen) {
        hasUnread = true;
        updateHandleState();
      }
    } catch (_) { /* offline / server down — try again next tick */ }
  }

  // ---------- notification sound ----------
  //
  // One shared HTMLAudioElement per page load — cheap, reused for every
  // ping. Settings are read from the cached GarageSettings on each play so
  // the user can flip the toggle / drag the slider and the next incoming
  // message respects it immediately.
  let notifAudio = null;
  function ensureNotifAudio() {
    if (notifAudio) return notifAudio;
    try {
      notifAudio = new Audio("/sounds/message.mp3");
      notifAudio.preload = "auto";
    } catch (_) {}
    return notifAudio;
  }
  async function playNotificationSound() {
    try {
      const s = window.GarageSettings ? await window.GarageSettings.get() : {};
      if (s.chat_sound_enabled === false) return;
      const vol = Math.max(0, Math.min(100, Number(s.chat_sound_volume)));
      if (!Number.isFinite(vol) || vol <= 0) return;
      const a = ensureNotifAudio();
      if (!a) return;
      a.volume = vol / 100;
      // Rewind if a previous ping is still finishing — otherwise a rapid
      // second arrival is silently swallowed.
      try { a.currentTime = 0; } catch (_) {}
      // play() returns a promise that rejects on browsers that block audio
      // before a user gesture. That's expected on first page load — the
      // sound will start working once the user clicks anywhere.
      const p = a.play();
      if (p && typeof p.catch === "function") p.catch(() => {});
    } catch (_) {}
  }

  // Full refresh: throws away the local cache and re-reads everything from
  // the server. Called when the chat is first opened (so the user sees the
  // whole history) and after we know something was pruned server-side.
  async function reloadFeed() {
    try {
      const res = await fetch("/api/chat/feed");
      if (!res.ok) { feed = []; render(); return; }
      const data = await res.json();
      feed = Array.isArray(data.items) ? data.items : [];
      maxTsSeen = feed.reduce((m, it) => Math.max(m, Number(it.ts) || 0), 0);
      render();
    } catch (_) { feed = []; render(); }
  }

  // Two polling cadences share one timer — swap it out on open/close.
  function startPolling(fast) {
    stopPolling();
    const delay = fast ? POLL_OPEN_MS : POLL_CLOSED_MS;
    pollTimerId = setInterval(fetchNewMessages, delay);
  }
  function stopPolling() {
    if (!pollTimerId) return;
    clearInterval(pollTimerId);
    pollTimerId = null;
  }

  // Add / remove the "you have unread" class on the handle button. CSS
  // handles the colour flip and the vertical figure-8 shake animation.
  function updateHandleState() {
    const btn = document.getElementById("chat-handle");
    if (!btn) return;
    btn.classList.toggle("has-unread", hasUnread && !isOpen);
  }

  // ---------- shared settings cache (used by chat + event triggers) ----------
  //
  // Every page that fires chat events (job.html on Work done, store-item.html
  // on save, …) needs to read the chat_notify_* toggles first. Fetching
  // /api/settings on each event would be wasteful — cache it once per page
  // load and expose a small API so callers can `await GarageSettings.get()`.
  // reset() forces a refetch after the user saves new settings.
  window.GarageSettings = window.GarageSettings || (function () {
    let cached = null;
    let pending = null;
    async function get() {
      if (cached) return cached;
      if (pending) return pending;
      pending = (async () => {
        try {
          const r = await fetch("/api/settings");
          cached = r.ok ? await r.json() : {};
        } catch (_) { cached = {}; }
        pending = null;
        return cached;
      })();
      return pending;
    }
    function reset() { cached = null; pending = null; }
    return { get, reset };
  })();

  // Retention window in milliseconds. Zero (or all-zero fields) = "no limit".
  function retentionMs(s) {
    const m = Math.max(0, Number(s.chat_retention_months) || 0);
    const d = Math.max(0, Number(s.chat_retention_days)   || 0);
    const h = Math.max(0, Number(s.chat_retention_hours)  || 0);
    const days = m * 30 + d; // months approximated to 30 days each
    const totalHours = days * 24 + h;
    return totalHours * 3600 * 1000;
  }

  // Display-time retention filter. The SERVER now handles physical pruning
  // (see apply_chat_retention in main.rs) whenever `chat_retention_delete`
  // is on. This client-side filter only hides messages past the window
  // when delete=false — matches the setting's "hide but keep" semantics.
  async function applyRetention(feed) {
    let s;
    try { s = await window.GarageSettings.get(); } catch (_) { return feed; }
    if (!s) return feed;
    const win = retentionMs(s);
    if (win <= 0) return feed; // "no limit"
    const cutoff = Date.now() - win;
    return feed.filter(it => Number(it.ts || 0) >= cutoff);
  }

  // seedFeed removed — the feed lives on the server now, empty by default.

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

      /* "You have unread" state — inverted colour scheme (white handle
         with blue text) plus a subtle vertical figure-8 shake so the tab
         waves at the user from the corner of the eye, and a 2 Hz colour
         flicker that snaps between normal and inverted so the tab
         actively winks at the user.  The static background / color rules
         below apply if animation is disabled by the OS (prefers-reduced-
         motion); otherwise the invert keyframes take over. */
      #garage-chat .chat-handle.has-unread {
        background: #fff;
        color: #3275ac;
        animation:
          chat-handle-shake 1.6s ease-in-out infinite,
          chat-handle-invert 1s steps(1, end) infinite;
      }
      #garage-chat .chat-handle.has-unread:hover { background: #eef4fb; }

      /* Two-state hard flip: 0-500 ms inverted (white / blue), then
         500-1000 ms back to the normal blue-on-white palette.  steps(1)
         easing makes each half-cycle jump instead of fade, so it reads
         as blinking, not pulsing. */
      @keyframes chat-handle-invert {
        0%   { background: #fff;    color: #3275ac; }
        50%  { background: #3275ac; color: #fff;    }
        100% { background: #fff;    color: #3275ac; }
      }

      /* Vertical infinity (∞ rotated 90°) — 8 samples around the lemniscate,
         each keyframe includes the base translateY(-50%) so the handle
         stays vertically centred on the panel edge. Amplitude 5 px, so it
         looks like a soft wiggle rather than a spasm. */
      @keyframes chat-handle-shake {
        0%,   100% { transform: translate(0px,     calc(-50% - 5px)); }
        12.5%      { transform: translate(3px,     calc(-50% - 2px)); }
        25%        { transform: translate(0px,     calc(-50% + 0px)); }
        37.5%      { transform: translate(-3px,    calc(-50% + 2px)); }
        50%        { transform: translate(0px,     calc(-50% + 5px)); }
        62.5%      { transform: translate(3px,     calc(-50% + 2px)); }
        75%        { transform: translate(0px,     calc(-50% + 0px)); }
        87.5%      { transform: translate(-3px,    calc(-50% - 2px)); }
      }

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
      /* People-online pill centred between the title and the close X.
         .dot is a green presence LED, .n is the count. Updated on every
         poll response. When the count is unknown (haven't polled yet)
         we render an em-dash so the box doesn't jump width later. */
      .chat-head .chat-online {
        margin: 0 auto;
        display: inline-flex; align-items: center; gap: 6px;
        padding: 3px 10px;
        border-radius: 12px;
        background: rgba(255,255,255,0.15);
        color: #fff; font-size: 0.85rem; font-weight: 600;
        min-width: 3.2em; justify-content: center;
        line-height: 1;
      }
      .chat-head .chat-online .dot {
        width: 8px; height: 8px; border-radius: 50%;
        background: #48d16b;
        box-shadow: 0 0 6px #48d16b;
      }
      .chat-head .chat-online .n { font-variant-numeric: tabular-nums; }
      /* Close button — perfectly round, white background so the red glyph
         pops, and inline-flex centring so the × sits exactly in the middle
         of the circle regardless of the browser's default button metrics.
         padding:0 kills the browser default (which was inflating the
         height and making the circle look oval before). */
      .chat-head .chat-close {
        width: 32px; height: 32px;
        padding: 0;
        border: none;
        border-radius: 50%;
        background: #fff;
        color: #d64545;
        font-size: 22px; font-weight: 700; line-height: 1;
        font-family: inherit;
        cursor: pointer;
        display: inline-flex;
        align-items: center;
        justify-content: center;
        flex-shrink: 0;
      }
      .chat-head .chat-close:hover { background: #f2f2f2; color: #b02a2a; }

      .chat-feed {
        flex: 1; overflow-y: auto;
        padding: 12px; background: #f5f5f5;
        display: flex; flex-direction: column; gap: 8px;
        align-items: stretch;
      }
      .chat-empty { color: #888; font-size: 13px; text-align: center; margin-top: 20px; }

      /* Day separator — a small pale chip inserted between messages whose
         local dates differ. Purposely low-contrast so it reads as
         navigation, not content. Centred via align-self (the feed is a
         flex column) and clipped to at most ~1/3 of the panel width. */
      .chat-daysep {
        align-self: center;
        padding: 3px 12px;
        margin: 6px 0 4px;
        max-width: 70%;
        background: rgba(200, 200, 200, 0.30);
        color: #999;
        border-radius: 999px;
        font-size: 11px;
        font-weight: 600;
        letter-spacing: 0.03em;
        text-align: center;
        white-space: nowrap;
      }

      /* Every row explicitly stretches to the panel width. */
      .chat-item {
        align-self: stretch;
        width: 100%;
        min-width: 0;
        box-sizing: border-box;
        font-size: 13px; line-height: 1.4;
      }
      .chat-item .when { color: #999; font-size: 11px; }

      .chat-item.msg .bubble {
        background: #fff; border: 1px solid #ddd; border-radius: 8px;
        padding: 7px 10px;
      }
      .chat-item.msg .who { color: #3275ac; font-weight: 600; margin-right: 6px; }
      .chat-item.msg .text { white-space: pre-wrap; overflow-wrap: break-word; }

      /* --- Event bubble: Flexbox ---
         Row: [ico] | [ev-text]
         - ico is fixed-size (flex-basis auto, no grow/shrink)
         - ev-text takes all remaining space (flex-basis 0, grow, min-width 0)
         The direct-child selectors (bubble greater-than child) keep the
         rules from accidentally cascading into anything nested inside
         .ev-text. */
      .chat-item.event .bubble {
        background: #fff7e6; border: 1px solid #f0d9a8;
        border-left: 4px solid #e6a700; border-radius: 6px;
        padding: 7px 10px;
        display: flex;
        flex-direction: row;
        align-items: flex-start;
        gap: 8px;
        width: 100%;
        box-sizing: border-box;
      }
      .chat-item.event .bubble > .ico {
        flex: 0 0 auto;
        font-size: 15px; line-height: 1.3;
      }
      .chat-item.event .bubble > .ev-text {
        flex: 1 1 0;
        min-width: 0;
        overflow-wrap: break-word;
      }
      .chat-item.event .ev-kind {
        color: #a07400; font-weight: 600; font-size: 11px;
        text-transform: uppercase; letter-spacing: 0.04em;
      }
      /* Links inside event text — same blue as the rest of the app so
         chat entries feel native, not markdown-y. */
      .chat-item.event .ev-text a {
        color: #3275ac; font-weight: 600; text-decoration: none;
      }
      .chat-item.event .ev-text a:hover { text-decoration: underline; }

      .chat-compose {
        border-top: 1px solid #ddd; padding: 10px; background: #fff;
        display: flex; flex-direction: column; gap: 6px;
      }
      .chat-compose input[type="text"], .chat-compose textarea {
        font-family: inherit; font-size: 15px; line-height: 1.4;
        border: 1px solid #bbb; border-radius: 4px;
        padding: 8px 10px; width: 100%;
        box-sizing: border-box;
      }
      /* min-height = one line of 15px font at line-height 1.4 (≈ 21px) plus
         our 8px top + 8px bottom padding = 37px. The textarea starts at
         exactly one line's height, so a fresh (empty) field vertically
         centres its placeholder / caret instead of leaving dead space at
         the bottom. The JS input listener grows it as the user types. */
      .chat-compose textarea {
        resize: none; min-height: 37px; max-height: 120px;
      }
      .chat-compose .row { display: flex; gap: 6px; align-items: stretch; }
      .chat-compose button {
        font-family: inherit; font-size: 15px; font-weight: 600;
        border: 1px solid #3275ac; background: #3275ac; color: #fff;
        border-radius: 4px; padding: 8px 16px; cursor: pointer;
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

  // `feed` is declared up in the server-side feed section — don't
  // redeclare it here, that would be a SyntaxError inside the IIFE.
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
          <span class="chat-online" id="chat-online" title="People with the app open in the last 10 minutes">
            <span class="dot"></span><span class="n">–</span>
          </span>
          <button class="chat-close" id="chat-close" aria-label="Close">×</button>
        </div>
        <div class="chat-feed" id="chat-feed"></div>
        <div class="chat-compose">
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

  // Local-date key used to detect day boundaries between two items.
  // Local (not UTC) so a message posted at 23:30 doesn't share a day with
  // one posted 45 minutes later in the same physical evening.
  function dayKey(ts) {
    const d = new Date(ts);
    return d.getFullYear() + "-" + (d.getMonth() + 1) + "-" + d.getDate();
  }

  // Day-separator label — fixed-width numeric date plus the full weekday
  // name in English. Format: "DD.MM.YYYY Weekday" (e.g. "03.07.2026 Friday").
  // No "Today"/"Yesterday" — the mechanic reads a fixed shape every time,
  // so the eye doesn't have to re-parse a different label per day.
  function formatDaySepLabel(ts) {
    const d = new Date(ts);
    const pad2 = n => String(n).padStart(2, "0");
    const day   = pad2(d.getDate());
    const month = pad2(d.getMonth() + 1);
    const year  = d.getFullYear();
    // Force English weekday names so "Monday"/"Tuesday" always render the
    // same regardless of the browser's locale.
    let weekday;
    try {
      weekday = d.toLocaleDateString("en-GB", { weekday: "long" });
    } catch (_) {
      weekday = ["Sunday","Monday","Tuesday","Wednesday","Thursday","Friday","Saturday"][d.getDay()];
    }
    return `${day}.${month}.${year} ${weekday}`;
  }

  // Per-worker notification filter. Reads the currently signed-in
  // worker's `chat_notify_*` prefs (fetched lazily by workerMuteSet)
  // and drops events whose `notify_key` is in the muted set. Messages
  // (no notify_key) and events without a key always pass through.
  function filterByWorkerPrefs(items) {
    const muted = workerMuteSet();
    if (!muted || !muted.size) return items;
    return items.filter(it => {
      const nk = it && typeof it.notify_key === "string" ? it.notify_key.trim() : "";
      return !nk || !muted.has(nk);
    });
  }

  // Set of notify_key strings the current worker has muted, or null
  // while we haven't fetched their record yet (in which case NOTHING
  // is muted — the mechanic sees the full feed). Cached until logout /
  // worker switch resets it.
  let mutedKeysCache = null;
  let mutedKeysFor  = "";
  function workerMuteSet() {
    const w = typeof window.currentWorker === "function" ? window.currentWorker() : null;
    const id = (w && w.id) || "";
    if (id !== mutedKeysFor) {
      mutedKeysCache = null;
      mutedKeysFor   = id;
      if (id) {
        fetch(`/api/workers/${encodeURIComponent(id)}`)
          .then(r => r.ok ? r.json() : null)
          .then(data => {
            if (!data || mutedKeysFor !== id) return;
            const s = new Set();
            const check = (key, flag) => { if (data[flag] === false) s.add(key); };
            check("job_started",   "chat_notify_job_started");
            check("job_finished",  "chat_notify_job_finished");
            check("job_reopened",  "chat_notify_job_reopened");
            check("stock_arrival", "chat_notify_stock_arrival");
            check("low_stock",     "chat_notify_low_stock");
            mutedKeysCache = s;
            // New prefs may hide items already on screen — repaint.
            try { render(); } catch (_) {}
          })
          .catch(() => {});
      }
    }
    return mutedKeysCache;
  }

  function render() {
    const list = document.getElementById("chat-feed");
    if (!list) return;
    // Retention is enforced server-side now — anything the server sent us
    // is fair game to display. We may still hide items past the window
    // when `delete=false` (server keeps everything, client just hides).
    applyRetention(feed).then(all => {
      const visible = filterByWorkerPrefs(all);
      if (!visible.length) {
        list.innerHTML = `<div class="chat-empty">No messages yet.</div>`;
        return;
      }
      // Walk items in order and inject a day-separator chip whenever the
      // local date changes (including before the very first visible item).
      const chunks = [];
      let lastKey = "";
      for (const it of visible) {
        const k = dayKey(it.ts || 0);
        if (k !== lastKey) {
          chunks.push(
            `<div class="chat-daysep">${esc(formatDaySepLabel(it.ts || 0))}</div>`
          );
          lastKey = k;
        }
        chunks.push(renderItem(it));
      }
      list.innerHTML = chunks.join("");
      list.scrollTop = list.scrollHeight;
    });
  }

  // Tiny markdown-style [label](/path) parser for event text. Only
  // same-site paths (must start with "/") are turned into <a> tags —
  // anything else is escaped verbatim so a stray external URL can't be
  // clicked into. Text outside link syntax is always HTML-escaped.
  function formatEventText(text) {
    const s = String(text || "");
    const re = /\[([^\]]+)\]\(([^)]+)\)/g;
    const out = [];
    let last = 0, m;
    while ((m = re.exec(s)) !== null) {
      if (m.index > last) out.push(esc(s.slice(last, m.index)));
      const [full, label, url] = m;
      if (url.startsWith("/")) {
        out.push(`<a href="${esc(url)}">${esc(label)}</a>`);
      } else {
        out.push(esc(full));
      }
      last = re.lastIndex;
    }
    if (last < s.length) out.push(esc(s.slice(last)));
    return out.join("");
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
              <div>${formatEventText(it.text)}</div>
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

  // Post a new message to the server. On success, appends the returned
  // record (with server-assigned id/ts) to the local cache and re-renders
  // optimistically — so the author sees their message right away without
  // waiting for the next poll.
  async function postMessage(text, author) {
    try {
      const res = await fetch("/api/chat/message", {
        method: "POST",
        headers: {"Content-Type": "application/json"},
        body: JSON.stringify({ text, author }),
      });
      if (!res.ok) return null;
      const data = await res.json();
      const it = data && data.item;
      if (it) {
        feed.push(it);
        const t = Number(it.ts) || 0;
        if (t > maxTsSeen) maxTsSeen = t;
        render();
      }
      return it;
    } catch (_) { return null; }
  }

  async function postEvent(text, kind, notify_key) {
    try {
      const res = await fetch("/api/chat/event", {
        method: "POST",
        headers: {"Content-Type": "application/json"},
        body: JSON.stringify({ text, kind, notify_key: notify_key || "" }),
      });
      if (!res.ok) return null;
      const data = await res.json();
      const it = data && data.item;
      if (it) {
        feed.push(it);
        const t = Number(it.ts) || 0;
        if (t > maxTsSeen) maxTsSeen = t;
        render();
      }
      return it;
    } catch (_) { return null; }
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
    postMessage(text, author);
    input.value = "";
    input.style.height = "auto";
    input.focus();
  }

  function open() {
    const el = document.getElementById("garage-chat");
    if (el) el.classList.add("open");
    isOpen = true;
    // Opening the chat = "I saw the new stuff" — kill the shake / colour flip.
    hasUnread = false;
    updateHandleState();
    // Full refresh so the newly-opened chat shows the current state, then
    // start the fast poll to catch anything that arrives while it's open.
    reloadFeed();
    startPolling(true);
    startInactivityTimer();
    const input = document.getElementById("chat-input");
    if (input) setTimeout(() => input.focus(), 150);
  }

  function close() {
    const el = document.getElementById("garage-chat");
    if (el) el.classList.remove("open");
    isOpen = false;
    // Downshift to the slow "just watch for pings" cadence.
    startPolling(false);
    stopInactivityTimer();
  }

  function toggle() { isOpen ? close() : open(); }

  // ---------- inactivity auto-close ----------
  //
  // When the chat is idle for `chatIdleMs()` it slides itself away. Timer
  // is reset by any mouse/keyboard activity inside the chat panel plus
  // arrival of new messages while it's still open. Only active while the
  // chat is open — closed chat doesn't need to track anything.
  let lastActivity = Date.now();
  let inactivityTimerId = null;

  // Chat-idle timeout — configurable via Settings → Chat:
  //   1. If "Base on screensaver timeout" is on AND screensaver_idle_minutes > 0:
  //        idle = screensaver_idle_minutes * (screensaver_pct / 100)
  //   2. Otherwise (checkbox off OR screensaver 0):
  //        idle = chat_autoclose_fallback_minutes
  // Result is clamped to at least 1 minute so the chat never closes
  // instantly on the first render tick.
  async function chatIdleMs() {
    const FALLBACK_MIN = 2;
    let mins = FALLBACK_MIN;
    try {
      if (window.GarageSettings && typeof window.GarageSettings.get === "function") {
        const s = (await window.GarageSettings.get()) || {};
        const useScr = s.chat_autoclose_use_screensaver !== false;
        const scrMins = Number(s.screensaver_idle_minutes);
        const fallback = Number.isFinite(Number(s.chat_autoclose_fallback_minutes))
          ? Number(s.chat_autoclose_fallback_minutes)
          : FALLBACK_MIN;
        if (useScr && Number.isFinite(scrMins) && scrMins > 0) {
          const pct = Number.isFinite(Number(s.chat_autoclose_screensaver_pct))
            ? Number(s.chat_autoclose_screensaver_pct)
            : 30;
          mins = Math.round(scrMins * pct / 100);
        } else {
          mins = fallback;
        }
      }
    } catch (_) {}
    return Math.max(1, mins) * 60 * 1000;
  }

  function bumpActivity() { lastActivity = Date.now(); }

  function startInactivityTimer() {
    if (inactivityTimerId) return;
    bumpActivity();
    inactivityTimerId = setInterval(async () => {
      const idle = Date.now() - lastActivity;
      if (idle >= await chatIdleMs()) close();
    }, 30 * 1000); // check every 30 s — fine-grained enough for a minutes-level threshold
  }
  function stopInactivityTimer() {
    if (!inactivityTimerId) return;
    clearInterval(inactivityTimerId);
    inactivityTimerId = null;
  }

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

    // Manual event buttons (Check-in / Wheels in / Parts in / Done) removed
    // — workshop events now come from the app itself (job.html Work done,
    // store-item.html save, etc.) gated by Settings → Chat toggles, so the
    // manual quick-log doesn't add value here.

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && isOpen) close();
    });

    // Any interaction inside the chat panel resets the inactivity timer,
    // so the chat only auto-closes when the user actually walks away.
    const panel = document.getElementById("garage-chat");
    if (panel) {
      ["mousemove", "mousedown", "keydown", "wheel", "touchstart"].forEach(evt => {
        panel.addEventListener(evt, bumpActivity, { passive: true });
      });
    }
  }

  function install() {
    if (document.getElementById("garage-chat")) return;
    if (!document.body) return;
    // Wrap each step so a failure in one doesn't kill the whole install.
    // At minimum, injectMarkup must run so the handle button is on the page.
    try { injectStyles(); } catch (e) { console.error("[chat.js] injectStyles failed:", e); }
    try { injectMarkup(); } catch (e) { console.error("[chat.js] injectMarkup failed:", e); return; }
    // The feed is server-backed now — start with an empty local cache and
    // baseline against whatever the server currently has so we don't
    // mistake the entire history for "unread on next fetch".
    feed = [];
    try { wire(); }       catch (e) { console.error("[chat.js] wire failed:", e); }
    try { render(); }     catch (e) { console.error("[chat.js] render failed:", e); }
    try { refreshComposeAccess(); } catch (e) { console.error("[chat.js] refreshComposeAccess failed:", e); }
    // Baseline the max ts we've already seen (from the server) — otherwise
    // the very first background poll would flag EVERY message as "new".
    (async () => {
      try {
        // Same URL builder as regular polls — this baseline fetch also
        // counts as an "I'm here" ping so the online tile is populated
        // before the first poll interval fires.
        const res = await fetch(feedUrl());
        if (res.ok) {
          const data = await res.json();
          renderOnlineCount(data.online);
          const items = Array.isArray(data.items) ? data.items : [];
          maxTsSeen = items.reduce((m, it) => Math.max(m, Number(it.ts) || 0), 0);
        }
      } catch (_) {}
      // Kick off the background (closed) poll so a fresh page load starts
      // watching for pings immediately.
      startPolling(false);
    })();
  }

  // ---------- public API ----------

  window.GarageChat = {
    open, close, toggle,
    // Returns a promise so callers that need the message to land before
    // navigating away (e.g. job.html Close & deliver) can await it.
    message(text, author) {
      if (!text) return Promise.resolve(null);
      return postMessage(String(text), author || "System");
    },
    event(text, kind, notify_key) {
      if (!text) return Promise.resolve(null);
      const k = EVENT_META[kind] ? kind : "generic";
      return postEvent(String(text), k, notify_key || "");
    },
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", install);
  } else {
    install();
  }
})();
