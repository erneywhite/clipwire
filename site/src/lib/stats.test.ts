// Проверка подсчёта загрузок: node src/lib/stats.test.ts (Node 22.18+ понимает TypeScript сам).
import assert from 'node:assert/strict';
import { countDownloads, latestOf, mergeStats, osOfAsset, showDownloads, showStars, STARS_MIN } from './stats.ts';

// Установщики считаются, служебные файлы автообновления — нет.
assert.equal(osOfAsset('Clipwire_0.9.7_windows_x64-setup.exe'), 'windows');
assert.equal(osOfAsset('Clipwire_0.1.0_x64-setup.exe'), 'windows'); // старое имя
assert.equal(osOfAsset('Clipwire_0.9.7_macos_universal.dmg'), 'macos');
assert.equal(osOfAsset('Clipwire_0.9.7_linux_amd64.deb'), 'linux');
assert.equal(osOfAsset('Clipwire_0.9.7_linux_amd64.AppImage'), 'linux');
for (const junk of ['latest.json', 'Clipwire_0.9.7_windows_x64-setup.exe.sig', 'Clipwire_0.9.7_macos_universal.app.tar.gz', 'Clipwire_0.9.7_linux_amd64.AppImage.sig']) {
  assert.equal(osOfAsset(junk), null, junk);
}

const releases = [
  { tag_name: 'v1.0.0-rc1', prerelease: true, assets: [{ name: 'Clipwire_1.0.0_windows_x64-setup.exe', download_count: 5 }] },
  {
    tag_name: 'v0.9.7',
    assets: [
      { name: 'Clipwire_0.9.7_windows_x64-setup.exe', download_count: 10 },
      { name: 'Clipwire_0.9.7_windows_x64-setup.exe.sig', download_count: 99 },
      { name: 'Clipwire_0.9.7_macos_universal.dmg', download_count: 3 },
      { name: 'Clipwire_0.9.7_linux_amd64.deb', download_count: 2 },
      { name: 'Clipwire_0.9.7_linux_amd64.AppImage', download_count: 4 },
      { name: 'latest.json', download_count: 500 },
    ],
  },
  { tag_name: 'v0.9.6', draft: true, assets: [{ name: 'Clipwire_0.9.6_macos_universal.dmg', download_count: 1000 }] },
];
// Пререлиз в загрузки входит (файлы скачивали), черновик нет; последний выпуск — без пререлизов.
assert.deepEqual(countDownloads(releases), { windows: 15, macos: 3, linux: 6 });
assert.equal(latestOf(releases), '0.9.7');

assert.equal(showStars(null), false);
assert.equal(showStars(STARS_MIN - 1), false);
assert.equal(showStars(STARS_MIN), true);
assert.equal(showDownloads(null), false);
assert.equal(showDownloads({ windows: 5, macos: 0, linux: 3 }), false);
assert.equal(showDownloads({ windows: 69, macos: 33, linux: 60 }), true);

// Слияние источников: свежий ответ, сохранённый в браузере, сборка.
const build = { stars: 10, downloads: { windows: 50, macos: 20, linux: 30 }, latest: '0.9.7' };
const stored = { stars: 40, downloads: { windows: 2000, macos: 900, linux: 700 }, latest: '1.0.0' };
// GitHub не ответил: берём сохранённое, а не старую сборку.
assert.deepEqual(mergeStats(null, stored, build), stored);
// Свежий ответ частичный (звёзды не пришли): звёзды из сохранённого, загрузки — максимум.
assert.deepEqual(mergeStats({ stars: null, downloads: { windows: 2100, macos: 850, linux: 710 }, latest: '1.0.1' }, stored, build), {
  stars: 40,
  downloads: { windows: 2100, macos: 900, linux: 710 },
  latest: '1.0.1',
});
// Звёзды берутся из самого свежего источника, даже если их стало меньше.
assert.equal(mergeStats({ stars: 38, downloads: null, latest: null }, stored).stars, 38);
// Версия — самая новая из известных (сравнение по числам, не по строкам).
assert.equal(mergeStats({ stars: null, downloads: null, latest: '0.10.0' }, { stars: null, downloads: null, latest: '0.9.9' }).latest, '0.10.0');
// Ничего не известно.
assert.deepEqual(mergeStats(null, undefined), { stars: null, downloads: null, latest: null });

console.log('stats: ok');
