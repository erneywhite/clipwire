// Версия для ссылок на скачивание берётся при сборке из Cargo.toml репозитория.
// В браузере main.ts дополнительно спрашивает GitHub о последнем выпуске и
// подменяет ссылки, если вышла версия новее: сайт заливается вручную и
// не должен устаревать с каждым выпуском.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { fetchStats, type Stats } from './stats';

export const REPO = 'erneywhite/clipwire';

export function buildVersion(): string {
  try {
    const cargo = readFileSync(fileURLToPath(new URL('../../../Cargo.toml', import.meta.url)), 'utf8');
    const m = cargo.match(/^version\s*=\s*"([^"]+)"/m);
    if (m) return m[1];
  } catch {
    // сборка вне репозитория — ниже запасное значение
  }
  return '0.10.1';
}

// Звёзды и загрузки при сборке: один запрос на обе страницы. GitHub не ответил —
// счётчиков в HTML нет, их дорисует браузер. GITHUB_TOKEN необязателен, он лишь
// снимает лимит в 60 запросов в час.
let statsPromise: Promise<Stats> | null = null;
export function buildStats(): Promise<Stats> {
  statsPromise ??= fetchStats(REPO, { token: process.env.GITHUB_TOKEN, timeoutMs: 10000 }).catch(() => ({
    stars: null,
    downloads: null,
    latest: null,
  }));
  return statsPromise;
}

export const assetUrl = (v: string, suffix: string) =>
  `https://github.com/${REPO}/releases/download/v${v}/Clipwire_${v}${suffix}`;
