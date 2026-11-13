<script lang="ts">
  // Предпросмотр одного файла: начало текста, PDF во встроенном просмотрщике
  // WebView, видео и аудио с плеером. Файл читается через asset-протокол, доступ
  // к нему Rust открывает только для файлов из истории и с подходящей сигнатурой.
  // Файл с другого устройства показывается так же, когда скачан (по пути в
  // кэше); пока не скачан — карточка с кнопкой «Скачать». Так же и свой файл,
  // который пропал с места, но есть на сервере. Файл больше порога и папка
  // приходят напрямую с устройства-источника, папка — архивом, который
  // распаковывается в кэш; скачанная папка показывается карточкой с путём.
  import { convertFileSrc } from '@tauri-apps/api/core'
  import { api, type FileInfo, type FilePreview } from '../lib/api'
  import { bytesLabel } from '../lib/format'
  import { canDownload, entryProgress, isDirect, isHere, progressLabel, type TransferMap } from '../lib/transfer'

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
{#if !here && (file.remote || canDownload(file))}
  <div class="box muted">
    <span class="name">{file.name}</span>
    {#if canDownload(file)}
      {#if isDirect(file)}
        <span>{file.is_dir ? 'Папка · ' : ''}напрямую с устройства «{device}» · {bytesLabel(file.size)}</span>
      {:else if file.remote}
        <span>{file.is_dir ? 'Папка' : 'Файл'} на устройстве «{device}» · {bytesLabel(file.size)}</span>
      {:else}
        <span>Файла нет на месте, копия есть на сервере · {bytesLabel(file.size)}</span>
      {/if}
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
  <div class="box muted">{file.is_dir ? 'Папки' : 'Файла'} нет на месте: {file.is_dir ? 'её' : 'его'} переместили или удалили.</div>
{:else if file.is_dir}
  <div class="box muted">
    <svg class="folder" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7a2 2 0 0 1 2-2h4l2 2h6a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z" /></svg>
    <span class="name">{file.name}</span>
    <span>Папка{file.size > 0 ? ` · ${bytesLabel(file.size)}` : ''}</span>
    <span class="path">{path}</span>
  </div>
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
  .folder {
    align-self: center;
    width: 40px;
    height: 40px;
    fill: none;
    stroke: var(--c-muted);
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
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
