// Проверка подсчёта загрузок: node src/lib/stats.test.ts (Node 22.18+ понимает TypeScript сам).
import assert from 'node:assert/strict';
import { countDownloads, latestOf, osOfAsset, showDownloads, showStars, STARS_MIN } from './stats.ts';

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

console.log('stats: ok');
