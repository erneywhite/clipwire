<script lang="ts">
  // Свой плеер для предпросмотра видео и аудио. Декодирует по-прежнему сам
  // <video>/<audio>, здесь только кнопки: встроенные элементы управления
  // каждый движок рисует по-своему (на Windows серые иконки не видны ни на
  // чёрном видео, ни на белой капсуле аудио, громкость прячется в узком окне).
  //
  // Элементы управления не берут фокус клавиатуры: пробел и стрелки остаются
  // у списка окна вставки (просмотр и переход между записями).
  import { onDestroy } from 'svelte'
  import { t } from '../lib/i18n'

  let {
    src,
    kind,
    onerror,
  }: {
    src: string
    kind: 'video' | 'audio'
    /** Формат не воспроизводится. */
    onerror?: () => void
  } = $props()

  const VOLUME_KEY = 'clipwire.volume'

  let media: HTMLMediaElement | undefined = $state()
  let box: HTMLElement | undefined = $state()
  let paused = $state(true)
  let waiting = $state(false)
  let current = $state(0)
  let duration = $state(0)
  let volume = $state(savedVolume())
  let muted = $state(false)
  let stopped = false

  const canFullscreen = $derived(
    kind === 'video' &&
      (document.fullscreenEnabled || !!(document as { webkitFullscreenEnabled?: boolean }).webkitFullscreenEnabled),
  )

  function savedVolume(): number {
    try {
      const v = Number(localStorage.getItem(VOLUME_KEY))
      return Number.isFinite(v) && v > 0 && v <= 1 ? v : 1
    } catch {
      return 1
    }
  }

  // Открыли — сразу играет.
  $effect(() => {
    const m = media
    if (!m) return
    m.volume = volume
    m.play().catch(() => {})
  })

  $effect(() => {
    if (media) {
      media.volume = volume
      media.muted = muted
    }
  })

  // Закрыли — сразу тишина: Chromium не глушит плеер, убранный со страницы,
  // он доигрывает, пока его не соберёт GC. Сброс источника вызывает error —
  // это не «формат не поддерживается».
  onDestroy(() => {
    const m = media
    if (!m) return
    stopped = true
    m.pause()
    m.removeAttribute('src')
    m.load()
  })

  function failed() {
    if (!stopped) onerror?.()
  }

  function toggle() {
    if (!media) return
    if (media.paused) media.play().catch(() => {})
    else media.pause()
  }

  function seek(e: Event) {
    const v = Number((e.currentTarget as HTMLInputElement).value)
    if (media && Number.isFinite(v)) media.currentTime = v
  }

  function setVolume(e: Event) {
    volume = Number((e.currentTarget as HTMLInputElement).value)
    muted = volume === 0
    try {
      if (volume > 0) localStorage.setItem(VOLUME_KEY, String(volume))
    } catch {
      // Без хранилища громкость просто не запомнится.
    }
  }

  function fullscreen() {
    const el = box as (HTMLElement & { webkitRequestFullscreen?: () => void }) | undefined
    if (!el) return
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {})
    else if (el.requestFullscreen) el.requestFullscreen().catch(() => {})
    else el.webkitRequestFullscreen?.()
  }

  /** Отпустили ползунок — фокус уходит, чтобы стрелки снова листали список. */
  function release(e: Event) {
    ;(e.currentTarget as HTMLElement).blur()
  }

  function clock(s: number): string {
    if (!Number.isFinite(s) || s < 0) s = 0
    const h = Math.floor(s / 3600)
    const m = Math.floor((s % 3600) / 60)
    const sec = Math.floor(s % 60)
    const mm = h ? String(m).padStart(2, '0') : String(m)
    return `${h ? `${h}:` : ''}${mm}:${String(sec).padStart(2, '0')}`
  }

  const progress = $derived(duration > 0 ? (current / duration) * 100 : 0)
  const level = $derived(muted ? 0 : volume)
</script>

<!-- Окно вставки прячется, когда теряет фокус: звук не должен играть в спрятанном окне. -->
<svelte:window onblur={() => media?.pause()} />

<div class="player {kind}" bind:this={box}>
  {#if kind === 'video'}
    <!-- svelte-ignore a11y_media_has_caption, a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <video
      bind:this={media}
      {src}
      preload="metadata"
      bind:paused
      bind:currentTime={current}
      bind:duration
      onwaiting={() => (waiting = true)}
      onplaying={() => (waiting = false)}
      oncanplay={() => (waiting = false)}
      onerror={failed}
      onclick={toggle}
    ></video>
  {:else}
    <audio
      bind:this={media}
      {src}
      preload="metadata"
      bind:paused
      bind:currentTime={current}
      bind:duration
      onwaiting={() => (waiting = true)}
      onplaying={() => (waiting = false)}
      oncanplay={() => (waiting = false)}
      onerror={failed}
    ></audio>
  {/if}

  <div class="controls">
    <input
      class="seek"
      type="range"
      tabindex="-1"
      min="0"
      max={duration || 0}
      step="any"
      value={current}
      style:--p="{progress}%"
      aria-label={t('Перемотка')}
      oninput={seek}
      onpointerup={release}
    />
    <div class="bar">
      <button
        class="btn"
        tabindex="-1"
        aria-label={paused ? t('Воспроизвести') : t('Пауза')}
        title={paused ? t('Воспроизвести') : t('Пауза')}
        onmousedown={(e) => e.preventDefault()}
        onclick={toggle}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          {#if paused}
            <path class="fill" d="M8 5.5v13l10.5-6.5z" />
          {:else}
            <path class="fill" d="M7 5h3.5v14H7zM13.5 5H17v14h-3.5z" />
          {/if}
        </svg>
      </button>

      <span class="time" class:waiting>{clock(current)} / {clock(duration)}</span>
      <span class="spacer"></span>


      <button
        class="btn"
        tabindex="-1"
        aria-label={level === 0 ? t('Включить звук') : t('Выключить звук')}
        title={level === 0 ? t('Включить звук') : t('Выключить звук')}
        onmousedown={(e) => e.preventDefault()}
        onclick={() => {
          if (level > 0) return (muted = true)
          muted = false
          if (volume === 0) volume = 1
        }}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path class="fill" d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z" />
          {#if level === 0}
            <path d="m15.5 9.5 5 5M20.5 9.5l-5 5" />
          {:else}
            <path d="M15.5 9.5a3.5 3.5 0 0 1 0 5" />
            {#if level > 0.5}<path d="M18 7a7 7 0 0 1 0 10" />{/if}
          {/if}
        </svg>
      </button>
      <input
        class="volume"
        type="range"
        tabindex="-1"
        min="0"
        max="1"
        step="0.05"
        value={level}
        style:--p="{level * 100}%"
        aria-label={t('Громкость')}
        oninput={setVolume}
        onpointerup={release}
      />

      {#if canFullscreen}
        <button
          class="btn"
          tabindex="-1"
          aria-label={t('Во весь экран')}
          title={t('Во весь экран')}
          onmousedown={(e) => e.preventDefault()}
          onclick={fullscreen}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5" /></svg>
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .player {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow: hidden;
    border-radius: var(--radius-m);
    border: 1px solid var(--c-border);
    background: var(--c-surface);
  }
  .player.video {
    flex: 1;
  }
  video {
    flex: 1;
    min-height: 0;
    width: 100%;
    background: #000;
    object-fit: contain;
    cursor: pointer;
  }
  .player:fullscreen video {
    cursor: default;
  }
  /* Две строки: перемотка на всю ширину, под ней кнопки и время. Так
     меняющаяся ширина времени (9:59 → 10:00) не двигает полосу под мышью. */
  .controls {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 4px var(--space-2) 6px;
    color: var(--c-text);
  }
  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .spacer {
    flex: 1;
  }
  .btn {
    flex: none;
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-s, 6px);
    background: transparent;
    color: inherit;
    cursor: pointer;
  }
  .btn:hover {
    background: var(--c-hover);
  }
  .btn svg {
    width: 18px;
    height: 18px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .btn svg .fill {
    fill: currentColor;
    stroke: none;
  }
  .time {
    flex: none;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .time.waiting {
    animation: pulse 1s ease-in-out infinite alternate;
  }
  @keyframes pulse {
    to {
      opacity: 0.4;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .time.waiting {
      animation: none;
    }
  }

  /* Ползунки: дорожка с закрашенной частью в акцентном цвете. Префикс
     -webkit- понимают и WebView2, и WebKit на macOS и Linux. */
  input[type='range'] {
    -webkit-appearance: none;
    appearance: none;
    height: 18px;
    margin: 0;
    padding: 0;
    background: transparent;
    cursor: pointer;
  }
  .seek {
    width: 100%;
  }
  .volume {
    flex: none;
    width: 64px;
  }
  input[type='range']::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(
      to right,
      var(--c-accent) 0 var(--p, 0%),
      var(--c-border) var(--p, 0%) 100%
    );
  }
  input[type='range']::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    margin-top: -4px;
    border-radius: 50%;
    border: 0;
    background: var(--c-accent);
  }
  input[type='range']:hover::-webkit-slider-thumb {
    transform: scale(1.2);
  }
</style>
