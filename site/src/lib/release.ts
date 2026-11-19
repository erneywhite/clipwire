// Версия для ссылок на скачивание берётся при сборке из Cargo.toml репозитория.
// В браузере main.ts дополнительно спрашивает GitHub о последнем выпуске и
// подменяет ссылки, если вышла версия новее: сайт заливается вручную и
// не должен устаревать с каждым выпуском.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const REPO = 'erneywhite/clipwire';

export function buildVersion(): string {
  try {
    const cargo = readFileSync(fileURLToPath(new URL('../../../Cargo.toml', import.meta.url)), 'utf8');
    const m = cargo.match(/^version\s*=\s*"([^"]+)"/m);
    if (m) return m[1];
  } catch {
    // сборка вне репозитория — ниже запасное значение
  }
  return '0.9.7';
}

export const assetUrl = (v: string, suffix: string) =>
  `https://github.com/${REPO}/releases/download/v${v}/Clipwire_${v}${suffix}`;
