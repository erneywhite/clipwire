import { defineConfig } from 'astro/config';
import { loadEnv } from 'vite';
import { fileURLToPath } from 'node:url';

// Адрес сайта берётся из site/.env (SITE_URL=https://…): он нужен для canonical,
// hreflang, sitemap и ссылок на картинку для соцсетей. Файл не в репозитории,
// образец — .env.example. Без адреса сайт собирается с относительными ссылками.
const root = fileURLToPath(new URL('.', import.meta.url));
const { SITE_URL } = loadEnv(process.env.NODE_ENV ?? 'production', root, '');

// Статический сайт: английская версия в корне, русская в /ru/.
export default defineConfig({
  site: SITE_URL || undefined,
  build: { format: 'directory' },
  trailingSlash: 'ignore',
});
