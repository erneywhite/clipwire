// Превью репозитория для GitHub (Settings → Social preview), 1280×640:
// знак и название слева, настоящий снимок окна вставки справа.
// Запуск из apps/desktop: node scripts/social-preview.mjs
// Снимки — docs/screenshots (снимаются с чистого экземпляра, см. docs/screenshots/README.md).
import { readFileSync, writeFileSync } from 'node:fs'
import { Resvg } from '@resvg/resvg-js'

const root = new URL('../../../', import.meta.url)
const file = (p) => new URL(p, root)
const dataUri = (p, type) => `data:${type};base64,${readFileSync(file(p)).toString('base64')}`

const icon = dataUri('docs/brand/clipwire-icon.svg', 'image/svg+xml')
const shot = dataUri('docs/screenshots/paste-window.png', 'image/png')
// Окно вставки 382×522, на превью — 1.05.
const sw = 382 * 1.05
const sh = 522 * 1.05
const sx = 1280 - sw - 110
const sy = (640 - sh) / 2

const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="640" viewBox="0 0 1280 640">
  <defs>
    <radialGradient id="glow" cx="0.78" cy="0.55" r="0.6">
      <stop offset="0" stop-color="#e2a63b" stop-opacity="0.18"/>
      <stop offset="1" stop-color="#e2a63b" stop-opacity="0"/>
    </radialGradient>
    <filter id="shadow" x="-10%" y="-10%" width="120%" height="130%">
      <feDropShadow dx="0" dy="18" stdDeviation="22" flood-color="#000" flood-opacity="0.55"/>
    </filter>
  </defs>
  <rect width="1280" height="640" fill="#16181b"/>
  <rect width="1280" height="640" fill="url(#glow)"/>
  <image href="${icon}" x="64" y="168" width="88" height="88"/>
  <text x="64" y="330" font-family="Segoe UI" font-weight="600" font-size="64" fill="#f2efe8">Clipwire</text>
  <text x="66" y="384" font-family="Segoe UI" font-size="26" fill="#b9b4a9">Shared clipboard between your computers</text>
  <text x="66" y="420" font-family="Segoe UI" font-size="26" fill="#b9b4a9">through your own server, end-to-end encrypted</text>
  <text x="66" y="478" font-family="Consolas" font-size="20" fill="#e2a63b">text · images · files</text>
  <g filter="url(#shadow)">
    <image href="${shot}" x="${sx}" y="${sy}" width="${sw}" height="${sh}"/>
  </g>
</svg>`

const png = new Resvg(svg, { font: { loadSystemFonts: true, defaultFontFamily: 'Segoe UI' } }).render().asPng()
writeFileSync(file('docs/brand/social-preview.png'), png)
console.log(`docs/brand/social-preview.png: ${png.length} байт`)
