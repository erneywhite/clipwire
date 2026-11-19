import { defineConfig } from 'astro/config';

// Статический сайт: английская версия в корне, русская в /ru/.
export default defineConfig({
  build: { format: 'directory' },
  trailingSlash: 'ignore',
});
