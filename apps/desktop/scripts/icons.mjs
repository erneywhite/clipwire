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
console.log(`иконка приложения: icons-src/icon-1024.png; иконок трея: ${n}`)
