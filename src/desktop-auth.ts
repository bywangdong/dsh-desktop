export const DESKTOP_AUTH_FRAGMENT = "dsh-desktop-token";

export function desktopEntryUrl(authenticatedUrl: string): string {
  const url = new URL(authenticatedUrl);
  const token = url.searchParams.get("token");
  if (url.pathname !== "/" || url.searchParams.size !== 1 || !token) {
    return authenticatedUrl;
  }

  url.search = "";
  url.hash = new URLSearchParams({ [DESKTOP_AUTH_FRAGMENT]: token }).toString();
  return url.href;
}
