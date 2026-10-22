(function () {
  "use strict";

  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

  function status(el, text) {
    var p = el.querySelector(".cast-status");
    if (!p) {
      p = document.createElement("p");
      p.className = "cast-status";
      el.appendChild(p);
    }
    p.textContent = text;
  }

  function lastEventTime(cast) {
    var lines = cast.trim().split("\n");
    for (var i = lines.length - 1; i > 0; i--) {
      try {
        var ev = JSON.parse(lines[i]);
        if (Array.isArray(ev) && typeof ev[0] === "number") return ev[0];
      } catch (e) {}
    }
    return 0;
  }

  function npt(seconds) {
    var s = Math.ceil(seconds);
    return "npt:" + Math.floor(s / 60) + ":" + String(s % 60).padStart(2, "0");
  }

  function mount(el, scene) {
    var cast = (window.CLANK_CASTS || {})[scene];
    if (typeof cast !== "string") {
      status(el, "Recording " + scene + " did not register its data.");
      return;
    }
    var still = reduceMotion.matches;
    var opts = {
      theme: "clank",
      fit: "width",
      terminalFontFamily: '"JetBrains Mono", ui-monospace, Menlo, monospace',
      terminalLineHeight: 1.25,
      autoPlay: false,
      preload: true,
      // Under reduced motion the reader sees where the scene ends, and presses play to watch it happen.
      poster: still ? npt(lastEventTime(cast)) : "npt:0:01"
    };
    var ready = document.fonts && document.fonts.load
      ? document.fonts.load('1em "JetBrains Mono"').catch(function () {})
      : Promise.resolve();
    ready.then(function () {
      el.textContent = "";
      var player = window.AsciinemaPlayer.create({ data: cast }, el, opts);
      if (still || !("IntersectionObserver" in window)) return;
      var seen = new IntersectionObserver(function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting && !reduceMotion.matches) {
            seen.disconnect();
            player.play();
          }
        });
      }, { threshold: 0.6 });
      seen.observe(el);
    });
  }

  // Casts arrive as script tags rather than fetch() because fetch of a local file fails on file://.
  function load(el) {
    var scene = el.getAttribute("data-cast");
    if (!/^[a-z0-9-]+$/.test(scene || "")) return;
    status(el, "Loading recording " + scene + "…");
    var s = document.createElement("script");
    s.src = "casts/" + scene + ".js";
    s.onload = function () { mount(el, scene); };
    s.onerror = function () {
      status(el, "Recording " + scene + " is missing from book/casts/.");
    };
    document.head.appendChild(s);
  }

  function start() {
    if (!window.AsciinemaPlayer) {
      document.querySelectorAll(".cast[data-cast]").forEach(function (el) {
        status(el, "The recording player did not load.");
      });
      return;
    }
    document.querySelectorAll(".cast[data-cast]").forEach(load);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
