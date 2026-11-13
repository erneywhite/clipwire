import { mount } from 'svelte'
import { getCurrentWindow } from '@tauri-apps/api/window'
import Settings from './settings/Settings.svelte'
import Popup from './popup/Popup.svelte'
import Preview from './settings/Preview.svelte'
import Editor from './editor/Editor.svelte'
import './styles/base.css'

// Это приложение, а не браузер: своё контекстное меню только в полях ввода,
// браузерные клавиши (обновить страницу, печать, поиск, назад) отключены.
document.addEventListener('contextmenu', (e) => {
  const t = e.target as HTMLElement | null
  // Меню «вырезать/копировать/вставить» — только в текстовых полях; у ползунков,
  // переключателей и остального интерфейса меню нет.
  const textual = t?.closest(
    'textarea, input:not([type=range]):not([type=checkbox]):not([type=radio]):not([type=button])',
  )
  if (!textual) e.preventDefault()
})
document.addEventListener(
  'keydown',
  (e) => {
    const ctrl = e.ctrlKey || e.metaKey
    const blocked =
      e.key === 'F5' ||
      e.key === 'F7' ||
      e.key === 'F12' ||
      (ctrl && ['KeyR', 'KeyF', 'KeyG', 'KeyU', 'KeyJ', 'KeyS'].includes(e.code)) ||
      (ctrl && e.shiftKey && ['KeyI', 'KeyC', 'KeyR'].includes(e.code)) ||
      (e.altKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight'))
    // Ctrl+P в окне вставки — «закрепить», его обрабатывает само окно; печать гасим везде.
    if (blocked || (ctrl && e.code === 'KeyP')) e.preventDefault()
  },
  true,
)

// Одна сборка фронтенда на все окна: что рисовать, решает метка окна.
const label = getCurrentWindow().label
const target = document.getElementById('app')!
const view = { popup: Popup, preview: Preview, editor: Editor }[label] ?? Settings
const app = mount(view, { target })

export default app
