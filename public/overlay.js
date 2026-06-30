// Shared full-page loading overlay with a spinning car-wheel preloader.
// Usage:
//   showOverlay();                     // shows the overlay
//   showOverlay("Searching mpmoil…");  // shows with a label
//   hideOverlay();                     // hides (ref-counted, safe for nested calls)
//   await withOverlay(async () => { ... });   // auto show/hide around an async fn
(function () {
  if (window.__overlayInstalled) return;
  window.__overlayInstalled = true;

  let refCount = 0;

  function injectStyles() {
    const css = `
      #global-overlay {
        position: fixed;
        inset: 0;
        background: rgba(0, 0, 0, 0.55);
        z-index: 9999;
        display: flex;
        align-items: center;
        justify-content: center;
        flex-direction: column;
        gap: 18px;
        opacity: 1;
        transition: opacity 0.15s ease;
      }
      #global-overlay.overlay-hidden {
        opacity: 0;
        pointer-events: none;
      }
      .wheel-spinner {
        width: 140px;
        height: 140px;
        animation: wheel-spin 1.2s linear infinite;
        filter: drop-shadow(0 4px 12px rgba(0,0,0,0.5));
      }
      .wheel-spinner .tire { fill: #1a1a1a; stroke: #3a3a3a; stroke-width: 2; }
      .wheel-spinner .tread { fill: none; stroke: #222; stroke-width: 2; stroke-dasharray: 6 4; }
      .wheel-spinner .rim   { fill: #cfd6dd; stroke: #6b7480; stroke-width: 1.5; }
      .wheel-spinner .spoke { stroke: #6b7480; stroke-width: 5; stroke-linecap: round; }
      .wheel-spinner .hub   { fill: #3275ac; stroke: #1d4d77; stroke-width: 2; }
      .wheel-spinner .lug   { fill: #cfd6dd; }
      @keyframes wheel-spin {
        from { transform: rotate(0deg); }
        to   { transform: rotate(360deg); }
      }
      #global-overlay .overlay-label {
        color: #fff;
        font-family: "JetBrains Mono", monospace;
        font-size: 1.05rem;
        font-weight: 600;
        letter-spacing: 0.04em;
        text-shadow: 0 1px 4px rgba(0,0,0,0.6);
        text-align: center;
        max-width: 80vw;
      }
    `;
    const s = document.createElement("style");
    s.textContent = css;
    document.head.appendChild(s);
  }

  function injectMarkup() {
    const wrap = document.createElement("div");
    wrap.id = "global-overlay";
    wrap.className = "overlay-hidden";
    wrap.setAttribute("aria-hidden", "true");
    wrap.innerHTML = `
      <svg class="wheel-spinner" viewBox="0 0 100 100" xmlns="http://www.w3.org/2000/svg">
        <!-- tire -->
        <circle class="tire" cx="50" cy="50" r="46" />
        <circle class="tread" cx="50" cy="50" r="42" />
        <!-- rim -->
        <circle class="rim" cx="50" cy="50" r="32" />
        <!-- spokes (5) -->
        <g>
          <line class="spoke" x1="50" y1="50" x2="50" y2="22" />
          <line class="spoke" x1="50" y1="50" x2="76" y2="42" />
          <line class="spoke" x1="50" y1="50" x2="66" y2="76" />
          <line class="spoke" x1="50" y1="50" x2="34" y2="76" />
          <line class="spoke" x1="50" y1="50" x2="24" y2="42" />
        </g>
        <!-- hub -->
        <circle class="hub" cx="50" cy="50" r="9" />
        <circle class="lug" cx="50" cy="50" r="2.5" />
      </svg>
      <div class="overlay-label" id="overlay-label"></div>
    `;
    document.body.appendChild(wrap);
  }

  function ensureInstalled() {
    if (document.getElementById("global-overlay")) return;
    if (!document.body) return;
    injectStyles();
    injectMarkup();
  }

  function setLabel(text) {
    const el = document.getElementById("overlay-label");
    if (el) el.textContent = text || "";
  }

  window.showOverlay = function (label) {
    ensureInstalled();
    refCount += 1;
    setLabel(label);
    const el = document.getElementById("global-overlay");
    if (el) el.classList.remove("overlay-hidden");
  };

  window.hideOverlay = function () {
    refCount = Math.max(0, refCount - 1);
    if (refCount === 0) {
      const el = document.getElementById("global-overlay");
      if (el) el.classList.add("overlay-hidden");
      setLabel("");
    }
  };

  window.withOverlay = async function (label, fn) {
    if (typeof label === "function") { fn = label; label = ""; }
    window.showOverlay(label);
    try {
      return await fn();
    } finally {
      window.hideOverlay();
    }
  };

  // Install on DOMContentLoaded; if it's already past, install now.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ensureInstalled);
  } else {
    ensureInstalled();
  }
})();
