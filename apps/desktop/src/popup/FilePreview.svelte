<script lang="ts">
  // Предпросмотр одного файла: начало текста, PDF во встроенном просмотрщике
  // WebView, видео и аудио с плеером. Файл читается через asset-протокол, доступ
  // к нему Rust открывает только на время просмотра.
  import { convertFileSrc } from '@tauri-apps/api/core'
  import { api, type FileInfo, type FilePreview } from '../lib/api'
  import { bytesLabel } from '../lib/format'

  let { file }: { file: FileInfo } = $props()

  let info = $state<FilePreview | null>(null)
  let failed = $state('')
  let mediaError = $state(false)
  const src = $derived(convertFileSrc(file.path))

  $effect(() => {
    const path = file.path
    info = null
    failed = ''
    mediaError = false
    api
      .filePreview(path)
      .then((p) => {
        if (path === file.path) info = p
      })
      .catch((e) => (failed = String(e)))
  })
</script>

{#if failed}
  <div class="box muted">{failed}</div>
{:else if !info}
  <div class="box muted">Открываю…</div>
{:else if !file.exists}
  <div class="box muted">Файла нет на месте: его переместили или удалили.</div>
{:else if info.kind === 'text'}
  <pre class="box text">{info.text}{#if info.truncated}<span class="cut">
… показано начало, весь файл — {bytesLabel(file.size)}</span>{/if}</pre>
{:else if info.kind === 'pdf'}
  <iframe class="box pdf" src={src} title={file.name}></iframe>
{:else if info.kind === 'video' && !mediaError}
  <!-- svelte-ignore a11y_media_has_caption -->
  <video class="box video" {src} controls preload="metadata" onerror={() => (mediaError = true)}></video>
{:else if info.kind === 'audio' && !mediaError}
  <div class="box audio">
    <span class="name">{file.name}</span>
    <audio {src} controls preload="metadata" onerror={() => (mediaError = true)}></audio>
  </div>
{:else}
  <div class="box muted">
    {#if mediaError}
      Этот формат встроенный плеер не воспроизводит. Откройте файл в своей программе через «Показать в папке».
    {:else}
      Для этого типа файла предпросмотра нет.
    {/if}
    <span class="path">{file.path}</span>
  </div>
{/if}

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
</style>
