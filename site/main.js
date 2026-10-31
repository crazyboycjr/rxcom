// rxcom's homepage: the theme toggle, a clip that plays while it is seen, and
// the copy buttons.

(function () {
  "use strict";
  var root = document.documentElement;
  var system = window.matchMedia("(prefers-color-scheme: dark)");

  // Light or dark. A choice that matches the system's goes back to following
  // the system, so nothing is stored for it.
  function systemTheme() { return system.matches ? "dark" : "light"; }
  function shownTheme() { return root.dataset.theme || systemTheme(); }
  var toggle = document.querySelector(".theme-toggle");
  function describe() {
    if (!toggle) return;
    var theme = shownTheme();
    var following = !root.dataset.theme;
    var other = theme === "dark" ? "light" : "dark";
    toggle.setAttribute("aria-pressed", theme === "dark" ? "true" : "false");
    toggle.title = (theme === "dark" ? "Dark" : "Light") + " theme" +
      (following ? ", as the system is" : ", chosen here") + ". Click for " + other + ".";
  }
  if (toggle) {
    toggle.addEventListener("click", function () {
      var next = shownTheme() === "dark" ? "light" : "dark";
      try {
        if (next === systemTheme()) {
          delete root.dataset.theme;
          localStorage.removeItem("rxcom-theme");
        } else {
          root.dataset.theme = next;
          localStorage.setItem("rxcom-theme", next);
        }
      } catch (e) {
        root.dataset.theme = next;
      }
      describe();
    });
  }
  if (system.addEventListener) system.addEventListener("change", describe);
  describe();

  // The clip plays while it is on screen. With motion to be reduced it never
  // starts by itself: it shows its poster and its controls.
  var videos = Array.prototype.slice.call(document.querySelectorAll("video.clip"));
  var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (still || !("IntersectionObserver" in window)) {
    videos.forEach(function (v) { v.controls = true; });
  } else {
    var seen = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        var v = entry.target;
        if (entry.isIntersecting) {
          var playing = v.play();
          if (playing && playing.catch) playing.catch(function () { v.controls = true; });
        } else {
          v.pause();
        }
      });
    }, { threshold: 0.35 });
    videos.forEach(function (v) { seen.observe(v); });
  }

  // Copy buttons: each copies the code of its card.
  document.querySelectorAll(".copy").forEach(function (button) {
    button.addEventListener("click", function () {
      var card = button.closest(".install-item");
      var code = card && card.querySelector("pre code");
      if (!code || !navigator.clipboard) return;
      navigator.clipboard.writeText(code.textContent).then(function () {
        button.textContent = "Copied";
        setTimeout(function () { button.textContent = "Copy"; }, 1600);
      }, function () {});
    });
  });
})();
