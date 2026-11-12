import { mount } from 'svelte'
import { getCurrentWindow } from '@tauri-apps/api/window'
import Settings from './settings/Settings.svelte'
import Popup from './popup/Popup.svelte'
import './styles/base.css'

// Одна сборка фронтенда на все окна: что рисовать, решает метка окна.
const label = getCurrentWindow().label
const target = document.getElementById('app')!
const app = mount(label === 'popup' ? Popup : Settings, { target })

export default app
