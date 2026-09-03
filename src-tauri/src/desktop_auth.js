(() => {
  const desktopAuthFragment = "dsh-desktop-token";
  if (
    window.location.protocol !== "http:" ||
    window.location.hostname !== "127.0.0.1" ||
    window.location.pathname !== "/" ||
    window.location.search !== ""
  ) {
    return;
  }

  const fragment = new URLSearchParams(window.location.hash.slice(1));
  const token = fragment.get(desktopAuthFragment);
  if (fragment.size !== 1 || !token) return;

  const authenticatedUrl = new URL(window.location.href);
  authenticatedUrl.hash = "";
  authenticatedUrl.searchParams.set("token", token);
  window.location.replace(authenticatedUrl);
})();
