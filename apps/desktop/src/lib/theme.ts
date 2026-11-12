// Применяет тему и режим из настроек к <html>. Режим «как в системе»
// следит за prefers-color-scheme и переключается вместе с ОС.
import type { Mode, Theme } from './api'

const media = window.matchMedia('(prefers-color-scheme: dark)')
let current: { theme: Theme; mode: Mode } = { theme: 'graphite', mode: 'system' }

function render() {
  const root = document.documentElement
  const resolved = current.mode === 'system' ? (media.matches ? 'dark' : 'light') : current.mode
  root.dataset.theme = current.theme
  root.dataset.mode = resolved
}

media.addEventListener('change', render)

export function applyTheme(theme: Theme, mode: Mode) {
  current = { theme, mode }
  render()
}

render()
