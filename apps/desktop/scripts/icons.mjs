// Рендер иконок из исходников знака (docs/brand/*.svg).
//
//   node scripts/icons.mjs          — PNG трея и иконка приложения 1024 px
//   npx tauri icon icons-src/icon-1024.png   — затем все размеры приложения
//
// Цвета трея: листы под тему панели задач, провод — состояние.
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'

const here = dirname(fileURLToPath(import.meta.url))
const brand = join(here, '../../../docs/brand')
const trayOut = join(here, '../src-tauri/icons/tray')
const srcOut = join(here, '../icons-src')
mkdirSync(trayOut, { recursive: true })
mkdirSync(srcOut, { recursive: true })

const render = (svg, size) =>
  new Resvg(svg, { fitTo: { mode: 'width', value: size } }).render().asPng()

// Иконка приложения — один большой PNG, остальное нарежет `tauri icon`.
const appSvg = readFileSync(join(brand, 'clipwire-icon.svg'), 'utf8')
writeFileSync(join(srcOut, 'icon-1024.png'), render(appSvg, 1024))

// Трей. На тёмной панели листы светлые, на светлой — тёмные; цвет провода
// подобран под фон панели, чтобы хватало контраста.
const trayTpl = readFileSync(join(brand, 'clipwire-tray.svg'), 'utf8')
const panels = {
  dark: {
    sheets: '#f2f2f2',
    wire: { online: '#3fbf7a', connecting: '#f0b030', error: '#e5484d', paused: '#8a8f98', local: '#4a9be0' },
  },
  light: {
    sheets: '#1b1b1b',
    wire: { online: '#1f9a5a', connecting: '#c98a0c', error: '#d0393e', paused: '#7a7f88', local: '#2f7fc8' },
  },
}
// Кадр «приглушённого» провода для пульсации (подключение, новое обновление).
const dim = (hex) => hex + '66'

let n = 0
for (const [panel, { sheets, wire }] of Object.entries(panels)) {
  for (const [state, color] of Object.entries(wire)) {
    for (const [suffix, c] of [['', color], ['-dim', dim(color)]]) {
      const svg = trayTpl.replaceAll('SHEETS', sheets).replaceAll('WIRE', c)
      writeFileSync(join(trayOut, `${panel}-${state}${suffix}.png`), render(svg, 32))
      n++
    }
  }
}
// Картинки установщика NSIS: он понимает только BMP (24 бита, без сжатия).
function bmp(rgba, w, h) {
  const row = Math.ceil((w * 3) / 4) * 4
  const size = 54 + row * h
  const b = Buffer.alloc(size)
  b.write('BM', 0)
  b.writeUInt32LE(size, 2)
  b.writeUInt32LE(54, 10)
  b.writeUInt32LE(40, 14)
  b.writeInt32LE(w, 18)
  b.writeInt32LE(h, 22) // положительная высота — строки снизу вверх
  b.writeUInt16LE(1, 26)
  b.writeUInt16LE(24, 28)
  b.writeUInt32LE(row * h, 34)
  b.writeInt32LE(2835, 38) // 72 dpi
  b.writeInt32LE(2835, 42)
  for (let y = 0; y < h; y++) {
    const off = 54 + (h - 1 - y) * row
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 4
      const a = rgba[i + 3] / 255 // фон у картинок непрозрачный, но на всякий случай — на белое
      const mix = (c) => Math.round(c * a + 255 * (1 - a))
      b[off + x * 3] = mix(rgba[i + 2])
      b[off + x * 3 + 1] = mix(rgba[i + 1])
      b[off + x * 3 + 2] = mix(rgba[i])
    }
  }
  return b
}
const nsisOut = join(here, '../src-tauri/installer')
mkdirSync(nsisOut, { recursive: true })
for (const [name, file] of [['sidebar', 'installer-sidebar.svg'], ['header', 'installer-header.svg']]) {
  // В двойном разрешении: NSIS вписывает картинку в свою область, и на экранах
  // с масштабом больше 100% уменьшенная картинка чётче растянутой.
  const img = new Resvg(readFileSync(join(brand, file), 'utf8'), { fitTo: { mode: 'zoom', value: 2 } }).render()
  writeFileSync(join(nsisOut, `${name}.bmp`), bmp(img.pixels, img.width, img.height))
}

console.log(`иконка приложения: icons-src/icon-1024.png; иконок трея: ${n}; картинки установщика: src-tauri/installer`)
