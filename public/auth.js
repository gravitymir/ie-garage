// Nominal (lightweight) authentication.
// On every page that includes this script, checks localStorage for a current
// worker. If absent, shows a modal asking to pick a name from the workers
// list and enter the password. Stores the resulting {id, first_name, ...}
// in localStorage so other pages can read it via window.currentWorker().
//
// Helpers exposed globally:
//   window.currentWorker()  -> { id, first_name, last_name, patronymic } | null
//   window.logout()         -> clears the stored worker and re-shows the modal
//   window.requireAuth()    -> shows the modal even if a worker is already set
(function () {
  if (window.__authInstalled) return;
  window.__authInstalled = true;

  const STORAGE_KEY = "current_worker";

  function readWorker() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (!raw) return null;
      return JSON.parse(raw);
    } catch (_) {
      return null;
    }
  }
  function writeWorker(w) {
    if (w) localStorage.setItem(STORAGE_KEY, JSON.stringify(w));
    else localStorage.removeItem(STORAGE_KEY);
  }

  window.currentWorker = readWorker;
  window.logout = function () {
    writeWorker(null);
    ensureInstalled();
    refreshUserbar();
    window.dispatchEvent(new CustomEvent("worker:logout"));
    openModal();
  };
  window.requireAuth = function () {
    ensureInstalled();
    openModal();
  };

  function injectStyles() {
    const css = `
      /* User bar in the top-right corner. Only shown on the home page
         (see refreshUserbar) — every other page just wastes screen-space
         with it and, when the panels get busy, it starts overlapping
         real content. Home page has the clock so there's always room.
         Design is a pill: worker name on the left, red logout arrow on
         the right, one click to sign out. */
      #auth-userbar {
        position: fixed;
        top: 10px;
        right: 12px;
        z-index: 8500;
        display: none;
        align-items: center;
        gap: 8px;
        height: 36px;
        padding: 0 12px 0 14px;
        border-radius: 18px;
        background: rgba(255,255,255,0.95);
        border: 1px solid #ccc;
        box-shadow: 0 2px 8px rgba(0,0,0,0.12);
        cursor: pointer;
        font-family: "JetBrains Mono", monospace;
        font-size: 13px;
        font-weight: 600;
        color: #333;
      }
      #auth-userbar.visible { display: inline-flex; }
      #auth-userbar:hover { background: #fde6e6; }
      #auth-userbar svg { display: block; color: #c33; flex: 0 0 auto; }
      #auth-userbar .auth-user-name {
        white-space: nowrap;
        color: #333;
      }
      @media print { #auth-userbar { display: none !important; } }

      #auth-modal-bg {
        position: fixed; inset: 0;
        background: rgba(0,0,0,0.55);
        z-index: 9500;
        display: none;
        align-items: center; justify-content: center;
        padding: 20px;
      }
      #auth-modal-bg.open { display: flex; }
      #auth-modal {
        background: #fff;
        border-radius: 10px;
        padding: 24px;
        max-width: 420px;
        width: 100%;
        box-shadow: 0 8px 32px rgba(0,0,0,0.35);
        font-family: "JetBrains Mono", monospace;
      }
      #auth-modal h2 {
        margin: 0 0 14px;
        font-size: 1.3rem;
        color: #3275ac;
      }
      #auth-modal .auth-field { margin-bottom: 12px; }
      #auth-modal label {
        display: block;
        font-size: 12px;
        color: #555;
        margin-bottom: 4px;
      }
      #auth-modal select, #auth-modal input {
        width: 100%;
        font-family: inherit;
        font-size: 15px;
        padding: 8px 10px;
        border: 1px solid #bbb;
        border-radius: 4px;
        box-sizing: border-box;
      }
      #auth-modal .auth-actions {
        display: flex;
        justify-content: flex-end;
        margin-top: 16px;
      }
      #auth-modal button {
        font-family: inherit;
        padding: 8px 18px;
        border: 1px solid #3275ac;
        background: #3275ac;
        color: #fff;
        border-radius: 4px;
        cursor: pointer;
        font-size: 14px;
      }
      #auth-modal button:hover { opacity: 0.9; }
      #auth-status { font-size: 13px; margin-top: 8px; color: #555; }
      #auth-status.err { color: #c33; }
    `;
    const s = document.createElement("style");
    s.textContent = css;
    document.head.appendChild(s);
  }

  function injectUserbar() {
    const bar = document.createElement("button");
    bar.id = "auth-userbar";
    bar.type = "button";
    bar.setAttribute("aria-label", "Sign out");
    bar.innerHTML = `
      <span class="auth-user-name"></span>
      <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" />
        <polyline points="16 17 21 12 16 7" />
        <line x1="21" y1="12" x2="9" y2="12" />
      </svg>
    `;
    document.body.appendChild(bar);
    bar.addEventListener("click", () => {
      if (!confirm("Sign out?")) return;
      window.logout();
    });
  }

  // True on the home page only. Everywhere else we hide the userbar so
  // it doesn't clip real UI in a corner where cards are packed close.
  function isHomePage() {
    const p = window.location.pathname || "/";
    return p === "/" || /\/index\.html?$/.test(p);
  }

  function refreshUserbar() {
    const bar = document.getElementById("auth-userbar");
    if (!bar) return;
    const w = readWorker();
    if (!w || !isHomePage()) { bar.classList.remove("visible"); return; }
    const name = `${w.first_name || ""} ${w.last_name || ""}`.trim() || w.id;
    const nameEl = bar.querySelector(".auth-user-name");
    if (nameEl) nameEl.textContent = name;
    bar.title = `Sign out (${name})`;
    bar.classList.add("visible");
  }

  function injectMarkup() {
    const wrap = document.createElement("div");
    wrap.id = "auth-modal-bg";
    wrap.innerHTML = `
      <div id="auth-modal" role="dialog" aria-modal="true" aria-labelledby="auth-title">
        <h2 id="auth-title">Sign in</h2>
        <div class="auth-field">
          <label for="auth-worker">Worker</label>
          <select id="auth-worker"><option value="">Loading…</option></select>
        </div>
        <div class="auth-field">
          <label for="auth-password">Password <span id="auth-pw-hint" style="color:#888;font-weight:normal;font-size:11px;"></span></label>
          <input type="password" id="auth-password" autocomplete="current-password" />
        </div>
        <div id="auth-status"></div>
        <div class="auth-actions">
          <button id="auth-login-btn" type="button">Login</button>
        </div>
      </div>
    `;
    document.body.appendChild(wrap);

    document.getElementById("auth-login-btn").addEventListener("click", doLogin);
    document.getElementById("auth-password").addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); doLogin(); }
    });
    // Do NOT close on backdrop click — login is required.
  }

  let installed = false;
  function ensureInstalled() {
    if (installed) return;
    if (!document.body) return;
    installed = true;
    injectStyles();
    injectMarkup();
    injectUserbar();
    refreshUserbar();
  }

  function setStatus(text, isErr) {
    const el = document.getElementById("auth-status");
    if (!el) return;
    el.textContent = text || "";
    el.classList.toggle("err", !!isErr);
  }

  // Cached workers list (populated when the dropdown loads).
  let cachedWorkers = [];

  function updatePasswordHint() {
    const hintEl = document.getElementById("auth-pw-hint");
    const sel = document.getElementById("auth-worker");
    if (!hintEl || !sel) return;
    const id = sel.value;
    if (!id) { hintEl.textContent = ""; return; }
    const w = cachedWorkers.find((x) => x.id === id);
    if (w && w.has_password === false) {
      hintEl.textContent = "(no password set — leave blank to sign in)";
    } else {
      hintEl.textContent = "";
    }
  }

  async function loadWorkersIntoDropdown() {
    const sel = document.getElementById("auth-worker");
    if (!sel) return [];
    try {
      const res = await fetch("/api/workers");
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json();
      const items = data.items || [];
      cachedWorkers = items;
      if (!items.length) {
        sel.innerHTML = `<option value="">(no workers yet — add one first)</option>`;
        return items;
      }
      sel.innerHTML = "";
      items.forEach((w) => {
        const o = document.createElement("option");
        o.value = w.id;
        const name = `${w.first_name || ""} ${w.last_name || ""}`.trim() || w.id;
        o.textContent = name;
        sel.appendChild(o);
      });
      sel.removeEventListener("change", updatePasswordHint);
      sel.addEventListener("change", updatePasswordHint);
      updatePasswordHint();
      return items;
    } catch (e) {
      sel.innerHTML = `<option value="">Error loading workers</option>`;
      setStatus("Could not load workers: " + e.message, true);
      return [];
    }
  }

  function openModal() {
    ensureInstalled();
    const bg = document.getElementById("auth-modal-bg");
    if (!bg) return;
    bg.classList.add("open");
    setStatus("");
    document.getElementById("auth-password").value = "";
    loadWorkersIntoDropdown().then(() => {
      setTimeout(() => {
        const sel = document.getElementById("auth-worker");
        if (sel && sel.options.length) sel.focus();
      }, 50);
    });
  }
  function closeModal() {
    const bg = document.getElementById("auth-modal-bg");
    if (bg) bg.classList.remove("open");
  }

  async function doLogin() {
    const workerId = document.getElementById("auth-worker").value;
    const password = document.getElementById("auth-password").value;
    if (!workerId) { setStatus("Pick a worker first.", true); return; }
    setStatus("Signing in…");
    try {
      const res = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ worker_id: workerId, password }),
      });
      if (res.status === 401) { setStatus("Invalid password.", true); return; }
      if (!res.ok) { setStatus("Error: " + (await res.text()), true); return; }
      const data = await res.json();
      writeWorker(data.worker);
      closeModal();
      refreshUserbar();
      // Let interested pages refresh anything that depends on the worker.
      window.dispatchEvent(new CustomEvent("worker:login", { detail: data.worker }));
    } catch (e) {
      setStatus("Network error: " + e.message, true);
    }
  }

  async function init() {
    ensureInstalled();
    if (readWorker()) return;
    // No saved worker — first check whether there are any workers at all.
    // If the database is empty (fresh install), don't gate the UI: the user
    // needs to be able to navigate to /workers.html and create the first one.
    try {
      const res = await fetch("/api/workers");
      if (res.ok) {
        const data = await res.json();
        if (!(data.items || []).length) {
          // Nothing to log into yet — quietly skip the modal.
          return;
        }
      }
    } catch (_) {
      // If the workers list fails, fall through to showing the modal anyway.
    }
    openModal();
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
