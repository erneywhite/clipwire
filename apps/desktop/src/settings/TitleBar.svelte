<script lang="ts">
  // Своя полоса заголовка вместо системной: знак, название, свернуть, развернуть,
  // закрыть. Перетаскивание и двойной клик (развернуть) — через data-tauri-drag-region.
  // На маке кнопки — системный «светофор» слева поверх полосы (titleBarStyle
  // Overlay в tauri.macos.conf.json), свои не рисуются.
  import { t } from '../lib/i18n'
  import { invoke } from '@tauri-apps/api/core'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { isMac } from '../lib/platform'

  let { title = 'Clipwire' }: { title?: string } = $props()
  const win = getCurrentWindow()
  let maximized = $state(false)

  $effect(() => {
    win.isMaximized().then((m) => (maximized = m))
    const un = win.onResized(async () => (maximized = await win.isMaximized()))
    return () => {
      un.then((f) => f())
    }
  })
</script>

<header class="bar" class:mac={isMac} data-tauri-drag-region>
  <span class="brand" data-tauri-drag-region>
    <svg viewBox="0 0 256 256" aria-hidden="true">
      <path class="sheet" d="M58 28 H118 L146 56 V148 Q146 166 128 166 H58 Q40 166 40 148 V46 Q40 28 58 28 Z" />
      <path class="fold" d="M118 28 V56 H146 Z" />
      <path class="sheet" d="M128 90 H188 L216 118 V210 Q216 228 198 228 H128 Q110 228 110 210 V108 Q110 90 128 90 Z" />
      <path class="fold" d="M188 90 V118 H216 Z" />
      <path class="wire" d="M72 66 V112 Q72 130 90 130 H146 Q164 130 164 148 V178" />
      <circle class="dot" cx="72" cy="64" r="16" />
      <circle class="dot" cx="164" cy="182" r="16" />
    </svg>
    {title}
  </span>
  {#if !isMac}
  <div class="controls">
    <button aria-label={t('Свернуть')} onclick={() => win.minimize()}>
      <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1 5h8" /></svg>
    </button>
    <button aria-label={maximized ? t('Восстановить') : t('Развернуть')} onclick={async () => (maximized = await invoke<boolean>('settings_toggle_maximize'))}>
      {#if maximized}
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M3 1h6v6M1 3h6v6H1z" /></svg>
      {:else}
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1h8v8H1z" /></svg>
      {/if}
    </button>
    <button class="close" aria-label={t('Закрыть')} onclick={() => win.close()}>
      <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1l-8 8" /></svg>
    </button>
  </div>
  {/if}
</header>

<style>
  .bar {
    flex: none;
    height: 36px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: var(--c-surface);
    border-bottom: 1px solid var(--c-border);
    padding-left: var(--space-3);
  }
  /* Место под «светофор» macOS (три кнопки по 12 px с промежутками). */
  .bar.mac {
    padding-left: 78px;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .brand svg {
    width: 18px;
    height: 18px;
  }
  .sheet {
    fill: none;
    stroke: var(--c-text);
    stroke-width: 16;
    stroke-linejoin: round;
  }
  .fold {
    fill: var(--c-text);
    stroke: var(--c-text);
    stroke-width: 16;
    stroke-linejoin: round;
  }
  .wire {
    fill: none;
    stroke: var(--c-brand-wire);
    stroke-width: 18;
    stroke-linecap: round;
  }
  .dot {
    fill: var(--c-brand-wire);
  }
  .controls {
    display: flex;
    height: 100%;
  }
  .controls button {
    width: 46px;
    height: 100%;
    border: 0;
    background: transparent;
    color: var(--c-muted);
    display: grid;
    place-items: center;
    cursor: default;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .controls button:hover {
    background: var(--c-hover);
    color: var(--c-text);
  }
  .controls button.close:hover {
    background: var(--c-danger);
    color: #fff;
  }
  .controls svg {
    width: 10px;
    height: 10px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.1;
  }
</style>
