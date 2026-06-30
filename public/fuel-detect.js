// Detect a car's fuel type from text fields (mpmoil variant name, model, etc).
// Returns one of: "diesel", "petrol", "hybrid", "electric", "petrol-electric", "unknown".
//
// Confidence is high (>99%) for mainstream European cars because manufacturers
// use stable suffixes/codes. For edge cases (very old cars, JDM imports,
// some Chinese brands) it falls back to "unknown".
(function () {
  if (window.detectFuelType) return;

  // Order matters: hybrid markers checked first, then electric, then diesel,
  // then petrol. Otherwise "Toyota Yaris Hybrid" might match petrol patterns.
  const HYBRID = [
    /\bphev\b/i, /\bmhev\b/i, /\bhev\b/i,
    /\bhybrid\b/i, /\bплаг-ин\b/i,
    /\be-hdi\b/i, /\be-power\b/i,
    /\bhsd\b/i,           // Toyota Hybrid Synergy Drive
  ];

  const ELECTRIC = [
    /\bbev\b/i, /\bev\b/i,
    /\belectric\b/i,
    /\be-tron\b/i, /\bid\.?\d?\b/i, /\biev\b/i,
  ];

  // Watch out: standalone "d" only catches BMW-style ("320d", "M340d") via
  // a word boundary against a digit on its left.
  const DIESEL = [
    /\bdpf\b/i, /\bad ?blue\b/i, /\bbluetec\b/i, /\becoblue\b/i,
    /\bdiesel\b/i,
    /\btdi\b/i, /\bsdi\b/i,
    /\btdci\b/i, /\bduratorq\b/i,
    /\bhdi\b/i, /\bbluehdi\b/i,
    /\bdci\b/i,
    /\bcdti?\b/i,
    /\bjtd(?:m)?\b/i, /\bmultijet\b/i, /\bmjet\b/i,
    /\bcdi\b/i, /\bbluetec\b/i,
    /\bcrdi?\b/i,
    /\bskyactiv-?d\b/i, /\bmzr-?cd\b/i,
    /\b[sst]dv6\b/i, /\b[sst]dv8\b/i, /\btdv6\b/i, /\btdv8\b/i, /\bed4\b/i,
    /\bd-?4-?d\b/i,
    /\bi-?dtec\b/i,
    /\bboxer\s*diesel\b/i,
    /\bdid\b/i,           // Mitsubishi DI-D
    // BMW style: digits then 'd' (e.g. "320d", "M340d", "X5 30d")
    /\b[mx]?\d{2,3}d\b/i,
  ];

  const PETROL = [
    /\btsi\b/i, /\btfsi\b/i, /\bfsi\b/i, /\bmpi\b/i,
    /\becoboost\b/i, /\bti-?vct\b/i, /\bsct[ie]?\b/i,
    /\bvti\b/i, /\bthp\b/i, /\bpuretech\b/i,
    /\bvvt-?i\b/i, /\bd-?4-?s\b/i,    // Toyota
    /\bgdi\b/i, /\bt-?gdi\b/i,
    /\bi?-?vtec\b/i,
    /\bskyactiv-?g\b/i, /\bdisi\b/i, /\bmzr(?!\s*-?cd)\b/i,
    /\bcgi\b/i, /\bblueefficiency\b/i,
    /\btce\b/i,                       // Renault TCe
    /\bturbo\s*petrol\b/i, /\bpetrol\b/i, /\bgasoline\b/i,
    // BMW style: digits then 'i' (e.g. "320i", "530i", "X5 40i")
    /\b[mx]?\d{2,3}i\b/i,
  ];

  function anyMatch(text, regexList) {
    for (const re of regexList) {
      if (re.test(text)) return true;
    }
    return false;
  }

  // Pull a peak RPM out of structured inputs OR free text ("5600RPM").
  function extractRpm(inputs, text) {
    for (const v of inputs) {
      if (v && typeof v === "object") {
        const r = v.rpm ?? v.peak_rpm ?? v.max_rpm;
        const n = Number(r);
        if (Number.isFinite(n) && n > 0) return n;
      }
    }
    const m = String(text).match(/(\d{4,5})\s*(?:rpm|об\/мин)/i);
    return m ? Number(m[1]) : null;
  }

  window.detectFuelType = function (...inputs) {
    const text = inputs
      .map((v) => {
        if (v == null) return "";
        if (typeof v === "string") return v;
        if (typeof v === "object") {
          return [v.name, v.title, v.label, v.model, v.brand]
            .filter(Boolean).join(" ");
        }
        return String(v);
      })
      .join(" ");
    if (!text.trim()) return "unknown";

    const isHybrid = anyMatch(text, HYBRID);
    const isElectric = anyMatch(text, ELECTRIC);
    const isDiesel = anyMatch(text, DIESEL);
    const isPetrol = anyMatch(text, PETROL);

    // Irish vehicle reg-cert categories: PETROL/ELECTRIC, DIESEL/ELECTRIC, etc.
    if (isHybrid && isDiesel)  return "diesel-electric"; // rare (E300 BlueTEC Hybrid, 3008 HYbrid4)
    if (isHybrid && isPetrol)  return "petrol-electric"; // typical (Prius, Yaris HSD, RAV4)
    if (isHybrid)              return "petrol-electric"; // hybrid w/o fuel marker — overwhelmingly petrol-based
    if (isElectric) return "electric";
    if (isDiesel) return "diesel";
    if (isPetrol) return "petrol";

    // ---------- RPM fallback ----------
    // mpmoil's variant payload includes the rated-power RPM. Diesel engines
    // almost never peak above ~4800 RPM; petrol engines almost always peak
    // at 5000+. So when no marker matched but we know the peak RPM:
    //   ≥ 5000  → petrol (confident)
    //   ≤ 4500  → diesel (likely, but less confident — small naturally-
    //             aspirated petrols sometimes peak there too, so we only
    //             call it diesel if RPM is genuinely low)
    const rpm = extractRpm(inputs, text);
    if (rpm) {
      if (rpm >= 5000) return "petrol";
      if (rpm <= 4500) return "diesel";
    }
    return "unknown";
  };

  // Friendly label + colour palette for UI usage. Labels match the wording
  // used on Irish Vehicle Registration Certificates.
  window.fuelTypeMeta = function (fuel) {
    switch (String(fuel || "").toLowerCase()) {
      case "diesel":          return { label: "DIESEL",          color: "#7a4f1d", bg: "#fdebd0" };
      case "petrol":          return { label: "PETROL",          color: "#1b6b3a", bg: "#d6f0df" };
      case "petrol-electric": return { label: "PETROL/ELECTRIC", color: "#1b5d7a", bg: "#d3eaf3" };
      case "diesel-electric": return { label: "DIESEL/ELECTRIC", color: "#6d3a8e", bg: "#e6dcf6" };
      case "electric":        return { label: "ELECTRIC",        color: "#0e4a9e", bg: "#d7e6fb" };
      // Legacy alias — treated as petrol-electric for display.
      case "hybrid":          return { label: "PETROL/ELECTRIC", color: "#1b5d7a", bg: "#d3eaf3" };
      default:                return { label: "UNKNOWN",         color: "#666",    bg: "#ececec" };
    }
  };
})();
