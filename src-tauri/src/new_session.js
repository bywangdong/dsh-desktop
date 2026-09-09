(() => {
  if (
    window.location.protocol !== "http:" ||
    window.location.hostname !== "127.0.0.1" ||
    window !== window.top
  ) return;

  // Use the upstream action so workspace selection, blank-session reuse and
  // draft persistence stay owned by DSH. Accessible labels survive CSS builds.
  const buttons = document.querySelectorAll(
    'button[aria-label="New session"], button[aria-label="新建会话"]'
  );
  const button = Array.from(buttons).find(candidate =>
    !candidate.disabled &&
    candidate.getAttribute("aria-disabled") !== "true" &&
    !candidate.closest('[inert], [aria-hidden="true"]') &&
    candidate.getClientRects().length > 0
  );
  if (!button) {
    console.warn("DSH Desktop: the New session action is not available on this page.");
    return;
  }
  button.click();
})();
