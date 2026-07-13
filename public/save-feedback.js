// Shared save-button feedback pattern.
//
// Every "Save" button in the app runs the same UX loop:
//   click → fetch → success or error → flash the outcome on the button
//   itself → revert to the original button after ~2 s.
//
// The button stays in place with the same box size — only its label,
// colour and disabled state change. Replaces the earlier mix of
// top-of-page toasts, banner divs, and inline "Saved" spans.
//
// How the "no shift" guarantee is built:
//   1. CSS min-width: 7ch on #save-btn / #save-car. Even a naked "Save"
//      button starts wide enough for "Saved" (5) and "Problem" (7), so
//      swapping the label never grows it.
//   2. On click we also freeze btn.offsetWidth via inline min-width.
//      That covers buttons whose default label is LONGER than the flash
//      label (e.g. "Save preferences", 16 chars) — without the freeze
//      the button would shrink to fit "Saved" during the flash.
//   3. A very long custom errLabel (e.g. "Lunch outside worktime") can
//      still push the button wider than either baseline. That's fine —
//      the growth happens on error, which is rare, and the alternative
//      (measuring every state up-front) added complexity we no longer
//      need now that CSS handles the standard case.
//
// Usage from a page script:
//   try {
//     const res = await fetch("/api/settings", { method: "PUT", body });
//     if (!res.ok) throw new Error(await res.text());
//     flashSaveStatus(saveBtn, "ok");
//   } catch (e) {
//     flashSaveStatus(saveBtn, "err");
//   }
//
// Options (all optional):
//   { ms: 2000, okLabel: "Saved", errLabel: "Problem" }

(function () {
  if (window.flashSaveStatus) return;

  const CSS = `
    /* Reserve enough width for the flash labels so a short button like
       "Save" (4) doesn't grow when it becomes "Saved" (5) or "Problem"
       (7). text-align: center keeps whichever label is showing neatly
       centred in the reserved slot. */
    #save-btn, #save-car {
      min-width: 6rem;
      text-align: center;
    }
    .save-flash {
      transition:
        background-color 0.2s ease,
        border-color     0.2s ease,
        color            0.2s ease;
      cursor: default !important;
    }
    .save-flash.ok {
      background-color: #1a7a1a !important;
      border-color:     #1a7a1a !important;
      color:            #fff    !important;
      opacity:          1       !important;
    }
    .save-flash.err {
      background-color: #c33 !important;
      border-color:     #c33 !important;
      color:            #fff !important;
      opacity:          1    !important;
    }
  `;
  const style = document.createElement("style");
  style.textContent = CSS;
  document.head.appendChild(style);

  // Cache each button's default innerHTML the first time we touch it,
  // so a stale flash-state can never be captured as "the default" on
  // rapid resave. dataset stores strings, which is fine even for
  // buttons that carry SVG children.
  function rememberDefault(btn) {
    if (btn.dataset.defaultHtml === undefined) {
      btn.dataset.defaultHtml = btn.innerHTML;
    }
  }

  window.flashSaveStatus = function (btn, kind, opts) {
    if (!btn) return;
    opts = opts || {};
    const ms       = typeof opts.ms === "number" ? opts.ms : 2000;
    const okLabel  = opts.okLabel  || "Saved";
    const errLabel = opts.errLabel || "Problem";

    rememberDefault(btn);
    if (btn._flashTimer) clearTimeout(btn._flashTimer);

    // Freeze at the current width so a longer default label like
    // "Save preferences" doesn't shrink to fit "Saved" during the flash.
    // For short defaults ("Save") the CSS min-width: 7ch above already
    // gives the button enough room for the flash text — this freeze is
    // just about NOT shrinking below that.
    if (btn._flashSize == null) {
      btn._flashSize = {
        prevMinWidth:  btn.style.minWidth,
        prevMinHeight: btn.style.minHeight,
      };
      btn.style.minWidth  = btn.offsetWidth  + "px";
      btn.style.minHeight = btn.offsetHeight + "px";
    }

    btn.disabled = true;
    btn.classList.remove("ok", "err");
    btn.classList.add("save-flash", kind === "err" ? "err" : "ok");
    // textContent (not innerHTML) so any accidental HTML in the labels
    // renders as literal text — no XSS surface via the label option.
    btn.textContent = kind === "err" ? errLabel : okLabel;

    btn._flashTimer = setTimeout(() => {
      btn.innerHTML = btn.dataset.defaultHtml;
      btn.disabled = false;
      btn.classList.remove("save-flash", "ok", "err");
      if (btn._flashSize) {
        btn.style.minWidth  = btn._flashSize.prevMinWidth;
        btn.style.minHeight = btn._flashSize.prevMinHeight;
        btn._flashSize = null;
      }
      btn._flashTimer = null;
    }, ms);
  };
})();
