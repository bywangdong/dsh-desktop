(() => {
  const allowedOrigin =
    window.location.protocol === "tauri:" ||
    (window.location.protocol === "http:" && window.location.hostname === "127.0.0.1");
  if (!allowedOrigin) return;

  const trafficLightHoverWidth = 80;
  const trafficLightHoverHeight = 36;

  const installWindowChrome = () => {
    const root = document.documentElement;
    if (root.hasAttribute("data-dsh-window-chrome")) return;
    root.setAttribute("data-dsh-window-chrome", "");
    root.setAttribute("data-tauri-drag-region", "false");

    // Tauri's deep regions exclude interactive descendants. Coordinate gating
    // limits dragging to the top strip without covering the page's hit targets.
    const updateDragRegion = (event) => {
      const inStrip = event.button === 0 && event.clientX >= 72 &&
        event.clientY >= 0 && event.clientY < 24;
      root.setAttribute("data-tauri-drag-region", inStrip ? "deep" : "false");
    };
    document.addEventListener("mousedown", updateDragRegion, { capture: true });
    document.addEventListener("mouseup", updateDragRegion, { capture: true });

    let trafficLightsVisible = false;
    let visibilityUpdates = Promise.resolve();
    const setTrafficLightsVisible = (visible) => {
      if (visible === trafficLightsVisible) return;
      trafficLightsVisible = visible;
      visibilityUpdates = visibilityUpdates
        .then(() => window.__TAURI__.core.invoke("set_traffic_lights_visible", { visible }))
        .catch(() => {
          if (trafficLightsVisible === visible) {
            trafficLightsVisible = false;
          }
        });
    };

    document.addEventListener(
      "pointermove",
      (event) => {
        setTrafficLightsVisible(
          event.clientX >= 0 && event.clientX < trafficLightHoverWidth &&
          event.clientY >= 0 && event.clientY < trafficLightHoverHeight
        );
      },
      { capture: true, passive: true }
    );
    window.addEventListener("blur", () => setTrafficLightsVisible(false));
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", installWindowChrome, { once: true });
  } else {
    installWindowChrome();
  }
})();
