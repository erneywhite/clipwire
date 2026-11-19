// Звёзды и загрузки с GitHub. Один и тот же подсчёт работает при сборке (цифры сразу
// в HTML) и в браузере (цифры обновляются без пересборки сайта).

export type OsKey = 'windows' | 'macos' | 'linux';
export type Downloads = Record<OsKey, number>;
export interface Stats {
  stars: number | null;
  downloads: Downloads | null;
  /** Последний выпуск без префикса v, например 0.9.7. */
  latest: string | null;
}

/** Звёзды показываем только от этого числа: «0 ★» отпугивает сильнее, чем пустое место. */
export const STARS_MIN = 25;
/** Загрузки по ОС показываем, если у каждой системы их хотя бы столько. */
export const DOWNLOADS_MIN = 1;

interface Asset {
  name: string;
  download_count: number;
}
interface Release {
  tag_name?: string;
  draft?: boolean;
  prerelease?: boolean;
  assets?: Asset[];
}

/**
 * Считаем только установщики: .exe, .dmg, .deb и .AppImage. Подписи (.sig), архивы
 * автообновления (.app.tar.gz) и latest.json не в счёт: их качает сама программа.
 * Автообновление на Windows и в AppImage всё равно качает установщик, поэтому это
 * загрузки, а не люди.
 */
export function osOfAsset(name: string): OsKey | null {
  if (/\.exe$/i.test(name)) return 'windows';
  if (/\.dmg$/i.test(name)) return 'macos';
  if (/\.(deb|appimage)$/i.test(name)) return 'linux';
  return null;
}

export function countDownloads(releases: Release[]): Downloads {
  const d: Downloads = { windows: 0, macos: 0, linux: 0 };
  for (const r of releases) {
    if (r.draft) continue;
    for (const a of r.assets ?? []) {
      const os = osOfAsset(a.name);
      if (os) d[os] += a.download_count || 0;
    }
  }
  return d;
}

export function latestOf(releases: Release[]): string | null {
  const r = releases.find((x) => !x.draft && !x.prerelease && x.tag_name);
  const v = r?.tag_name?.replace(/^v/, '') ?? '';
  return /^\d+\.\d+\.\d+$/.test(v) ? v : null;
}

export const showStars = (n: number | null): n is number => n !== null && n >= STARS_MIN;
export const showDownloads = (d: Downloads | null): d is Downloads =>
  d !== null && Math.min(d.windows, d.macos, d.linux) >= DOWNLOADS_MIN;

const semver = (v: string) => v.split('.').map(Number);
const newer = (a: string, b: string) => {
  const [x, y] = [semver(a), semver(b)];
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i];
  return false;
};

/**
 * Сводит цифры из нескольких источников, от самого свежего к самому старому:
 * ответ GitHub сейчас, последний удачный ответ в этом браузере, сборка сайта.
 * Загрузки только растут, поэтому по каждой ОС берётся наибольшее: если GitHub не
 * ответил, человек не увидит, как 2000 загрузок вдруг превратились в 50.
 * Звёзды могут и убывать, их берём из самого свежего источника, где они есть.
 */
export function mergeStats(...items: (Stats | null | undefined)[]): Stats {
  const known = items.filter((x): x is Stats => !!x);
  const out: Stats = { stars: null, downloads: null, latest: null };
  for (const s of known) {
    if (out.stars === null && s.stars !== null) out.stars = s.stars;
    if (s.latest && (!out.latest || newer(s.latest, out.latest))) out.latest = s.latest;
    if (s.downloads) {
      out.downloads ??= { windows: 0, macos: 0, linux: 0 };
      for (const os of ['windows', 'macos', 'linux'] as OsKey[]) out.downloads[os] = Math.max(out.downloads[os], s.downloads[os] || 0);
    }
  }
  return out;
}

/** Запрос к GitHub API: звёзды и выпуски (до 100, больше пока нет). */
export async function fetchStats(repo: string, opts: { token?: string; timeoutMs?: number } = {}): Promise<Stats> {
  const headers: Record<string, string> = { Accept: 'application/vnd.github+json' };
  if (opts.token) headers.Authorization = `Bearer ${opts.token}`;
  const get = async (path: string) => {
    const signal = typeof AbortSignal !== 'undefined' && 'timeout' in AbortSignal ? AbortSignal.timeout(opts.timeoutMs ?? 8000) : undefined;
    const r = await fetch(`https://api.github.com/repos/${repo}${path}`, { headers, signal });
    if (!r.ok) throw new Error(`GitHub ${r.status}`);
    return r.json();
  };
  const [repoInfo, releases] = await Promise.allSettled([get(''), get('/releases?per_page=100')]);
  const rel = releases.status === 'fulfilled' && Array.isArray(releases.value) ? (releases.value as Release[]) : null;
  return {
    stars: repoInfo.status === 'fulfilled' ? Number(repoInfo.value.stargazers_count) || 0 : null,
    downloads: rel ? countDownloads(rel) : null,
    latest: rel ? latestOf(rel) : null,
  };
}
