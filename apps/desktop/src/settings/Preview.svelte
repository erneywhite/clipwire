<script lang="ts">
  // Окно-пример для превью размера: окно вставки на примерах, во всё окно.
  import { onMount } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type Settings } from '../lib/api'
  import { applyTheme } from '../lib/theme'
  import PopupMock from './PopupMock.svelte'

  onMount(() => {
    api.settings().then((s) => applyTheme(s.theme, s.mode))
    const u = listen<Settings>('clipwire://settings-changed', (e) => applyTheme(e.payload.theme, e.payload.mode))
    return () => u.then((f) => f())
  })
</script>

<PopupMock fill />
