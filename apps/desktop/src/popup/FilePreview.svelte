<script lang="ts">
  // Предпросмотр одного файла: начало текста, PDF во встроенном просмотрщике
  // WebView, видео и аудио с плеером. Файл читается через asset-протокол, доступ
  // к нему Rust открывает только на время просмотра.
  // Файл с другого устройства показывается так же, когда скачан (по пути в
  // кэше); пока не скачан — карточка с кнопкой «Скачать».
  import { convertFileSrc } from '@tauri-apps/api/core'
  import { api, type FileInfo, type FilePreview } from '../lib/api'
  import { bytesLabel } from '../lib/format'
  import { canDownload, entryProgress, isHere, progressLabel, type TransferMap } from '../lib/transfer'

  let {
    file,
    device = '',
    transfers = {},
    ondownload,
  }: {
    file: FileInfo
    /** Имя устройства, с которого пришла запись. */
    device?: string
    transfers?: TransferMap
    ondownload?: () => void
  } = $props()

  const here = $derived(isHere(file))
  const path = $derived(file.local_path ?? '')
  const progress = $derived(entryProgress([file], transfers))
  const downloading = $derived(progress?.direction === 'down' && progress.active)

  let info = $state<FilePreview | null>(null)
  let failed = $state('')
  let mediaError = $state(false)
  let media: HTMLMediaElement | undefined = $state()
  const src = $derived(convertFileSrc(path))

  // Открыли — сразу играет. Закрыли — сразу тишина: Chromium не глушит
  // плеер, убранный со страницы, он доигрывает, пока его не соберёт GC.
  $effect(() => {
    const m = media
    if (!m) return
    m.play().catch(() => {})
    return () => stop(m)
  })

  function stop(m: HTMLMediaElement | undefined) {
    if (!m) return
    // Сброс источника вызывает error — это не «формат не поддерживается».
    m.dataset.stopped = '1'
    m.pause()
    m.removeAttribute('src')
    m.load()
  }

  function onMediaError(e: Event) {
    if (!(e.currentTarget as HTMLElement).dataset.stopped) mediaError = true
  }

  $effect(() => {
    const want = path
    const ready = here
    info = null
    failed = ''
    mediaError = false
    // Не скачанный файл читать нечем: показывается карточка, а не ошибка.
    if (!ready) return
    api
      .filePreview(want)
      .then((p) => {
        if (want === path) info = p
      })
      .catch((e) => (failed = String(e)))
  })
</script>

<!-- Окно вставки прячется, когда теряет фокус: звук не должен играть в спрятанном окне. -->
<svelte:window onblur={() => media?.pause()} />

<!-- Свой плеер на каждый файл: при переключении старый останавливается. -->
{#key path}
{#if file.remote && !here}
  <div class="box muted">
    <span class="name">{file.name}</span>
    {#if canDownload(file)}
      <span>Файл на устройстве «{device}» · {bytesLabel(file.size)}</span>
      {#if progress}
        <span class="bar" class:failed={!!progress.error} aria-hidden="true">
          <span style:width="{progress.percent}%"></span>
        </span>
        <span class="state" class:failed={!!progress.error} role="status">{progressLabel(progress)}</span>
      {/if}
      <button class="get" disabled={downloading} onclick={() => ondownload?.()}>
        {downloading ? 'Скачивается…' : progress?.error ? 'Скачать ещё раз' : 'Скачать'}
      </button>
    {:else}
      <span>{file.is_dir ? 'Папка' : bytesLabel(file.size)} · есть только на устройстве «{device}»</span>
    {/if}
  </div>
{:else if !here}
  <div class="box muted">Файла нет на месте: его переместили или удалили.</div>
{:else if failed}
  <div class="box muted">{failed}</div>
{:else if !info}
  <div class="box muted">Открываю…</div>
{:else if info.kind === 'text'}
  <pre class="box text">{info.text}{#if info.truncated}<span class="cut">
… показано начало, весь файл — {bytesLabel(file.size)}</span>{/if}</pre>
{:else if info.kind === 'pdf'}
  <iframe class="box pdf" src={src} title={file.name}></iframe>
{:else if info.kind === 'video' && !mediaError}
  <!-- svelte-ignore a11y_media_has_caption -->
  <video bind:this={media} class="box video" {src} controls preload="metadata" onerror={onMediaError}></video>
{:else if info.kind === 'audio' && !mediaError}
  <div class="box audio">
    <span class="name">{file.name}</span>
    <audio bind:this={media} {src} controls preload="metadata" onerror={onMediaError}></audio>
  </div>
{:else}
  <div class="box muted">
    {#if mediaError}
      Этот формат встроенный плеер не воспроизводит. Откройте файл в своей программе через «Показать в папке».
    {:else}
      Для этого типа файла предпросмотра нет.
    {/if}
    <span class="path">{path}</span>
  </div>
{/if}
{/key}

<style>
  .box {
    flex: 1;
    min-height: 0;
    margin: 0;
    border-radius: var(--radius-m);
    background: var(--c-bg);
    border: 1px solid var(--c-border);
  }
  .muted {
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: var(--space-2);
    padding: var(--space-4);
    color: var(--c-muted);
    font-size: var(--text-s);
    text-align: center;
  }
  .path {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    overflow-wrap: anywhere;
  }
  .text {
    overflow: auto;
    padding: var(--space-2) var(--space-3);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    line-height: 1.5;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--c-text);
    user-select: text;
  }
  .cut {
    color: var(--c-muted);
  }
  .pdf {
    width: 100%;
    background: #fff;
  }
  .video {
    width: 100%;
    background: #000;
    object-fit: contain;
  }
  .audio {
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: var(--space-3);
    padding: var(--space-4);
  }
  .audio audio {
    width: 100%;
  }
  .name {
    overflow-wrap: anywhere;
  }
  .muted .name {
    color: var(--c-text);
  }
  .bar {
    display: block;
    align-self: stretch;
    height: 3px;
    border-radius: 2px;
    background: var(--c-border);
    overflow: hidden;
  }
  .bar > span {
    display: block;
    height: 100%;
    background: var(--c-accent);
    transition: width 200ms var(--ease-out);
  }
  .bar.failed > span {
    background: var(--c-danger);
  }
  .state {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .state.failed {
    color: var(--c-danger);
  }
  .get {
    align-self: center;
    border: 1px solid var(--c-accent);
    background: var(--c-accent);
    color: var(--c-on-accent);
    border-radius: var(--radius-m);
    padding: 6px 12px;
    cursor: pointer;
    white-space: nowrap;
  }
  .get:disabled {
    opacity: 0.6;
    cursor: default;
  }
  @media (prefers-reduced-motion: reduce) {
    .bar > span {
      transition: none;
    }
  }
</style>
