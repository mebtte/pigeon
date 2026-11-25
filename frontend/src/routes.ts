export type Route =
  | { page: 'home' | 'login' | 'notFound' }
  | { page: 'sharing'; id: string; password: string };

const shareHash = /^#\/s\/([A-Za-z0-9_-]{6})(?:\?(.*))?$/;

export function readRoute(hash: string): Route {
  if (!hash || hash === '#/') return { page: 'home' };
  if (hash === '#/login') return { page: 'login' };
  const share = shareHash.exec(hash);
  if (!share) return { page: 'notFound' };

  const [, id, query] = share;
  const params = new URLSearchParams(query);
  return { page: 'sharing', id, password: params.get('p') ?? '' };
}

export function shareLinkWithPassword(link: string, password: string): string {
  const url = new URL(link);
  const id = shareHash.exec(url.hash)?.[1];
  if (!id) return url.href;
  // The fragment carries the password without including it in the HTTP URL.
  const query = password ? `?${new URLSearchParams({ p: password })}` : '';
  url.search = '';
  url.hash = `#/s/${id}${query}`;
  return url.href;
}
