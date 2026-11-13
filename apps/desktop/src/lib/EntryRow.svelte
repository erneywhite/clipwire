<script lang="ts">
  // Строка элемента истории. Общая для окна вставки и для превью в настройках,
  // чтобы превью выглядело ровно как настоящее окно.
  import type { Entry } from './api'
  import AppTag from './AppTag.svelte'
  import { ago, bytesLabel, filesLabel, filesMeta, isUrl, looksLikeCode, sizeLabel } from './format'
  import { allUploaded, entryProgress, isDirect, isGone, progressLabel, type TransferMap } from './transfer'

  let {
    item,
    index,
    selected = false,
    onselect,
    onpaste,
    onpin,
    onremove,
    onmenu,
    transfers = {},
  }: {
    item: Entry
    index: number
    selected?: boolean
    /** Передачи файлов по blob — для полосы прогресса. */
    transfers?: TransferMap
    onselect?: () => void
    onpaste?: () => void
    /** Закрепить или открепить кликом по булавке. */
    onpin?: () => void
    /** Удалить из истории кликом по корзине. */
    onremove?: () => void
    /** Правый клик: меню действий в этой точке окна. */
    onmenu?: (x: number, y: number) => void
  } = $props()

  const kind = $derived(
    item.kind === 'image' || item.kind === 'files'
      ? item.kind
      : isUrl(item.text)
        ? 'link'
        : looksLikeCode(item.text)
          ? 'code'
          : 'text',
  )
  const folder = $derived(item.files.length > 0 && item.files.every((f) => f.is_dir))
  const progress = $derived(kind === 'files' ? entryProgress(item.files, transfers) : null)
  // Файл, которого нет здесь, но который есть на сервере, не «пропал»: его можно
  // скачать. Пропал только свой файл без копии на сервере.
  const gone = $derived(item.files.some(isGone))
  const device = $derived(item.local ? 'Это устройство' : item.device_name || 'Другое устройство')
  // Мета после устройства и приложения-источника (его иконка — отдельным элементом).
  const meta = $derived(
    [
      ago(item.seen_ms),
      kind === 'image'
        ? `${item.width}×${item.height} · ${bytesLabel(item.bytes)}`
        : kind === 'files'
          ? filesMeta(item.files, item.bytes, item.width, item.height)
          : sizeLabel(item.chars, item.lines),
      ...(item.rich ? [item.rich] : []),
      ...(progress ? [progressLabel(progress)] : []),
      // По журналу загрузок (переживает перезапуск) и по событиям этого запуска.
      ...(!progress && kind === 'files' && allUploaded(item.files, transfers) ? ['↑ на сервере'] : []),
      // Файл больше порога ждёт на источнике: его скачают, только пока оно в сети.
      ...(!progress && kind === 'files' && item.files.some(isDirect) ? [`напрямую с устройства «${device}»`] : []),
    ].join(' · '),
  )
</script>

<li
  data-i={index}
  role="option"
  aria-selected={selected}
  class:sel={selected}
  onclick={onselect}
  ondblclick={onpaste}
  oncontextmenu={(e) => {
    if (!onmenu) return
    e.preventDefault()
    onselect?.()
    onmenu(e.clientX, e.clientY)
  }}
  onkeydown={() => {}}
>
  <span class="glyph" aria-hidden="true">
    {#if kind === 'image'}
      <svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="14" rx="2" /><path d="m4 16 5-5 4 4 2-2 5 5" /></svg>
    {:else if kind === 'files' && folder}
      <svg viewBox="0 0 24 24"><path d="M4 7a2 2 0 0 1 2-2h4l2 2h6a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z" /></svg>
    {:else if kind === 'files'}
      <svg viewBox="0 0 24 24"><path d="M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z" /><path d="M14 3v5h5" /></svg>
    {:else if kind === 'link'}
      <svg viewBox="0 0 24 24"><path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" /><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" /></svg>
    {:else if kind === 'code'}
      <svg viewBox="0 0 24 24"><path d="m8 8-4 4 4 4M16 8l4 4-4 4" /></svg>
    {:else}
      <svg viewBox="0 0 24 24"><path d="M5 7h14M5 12h14M5 17h9" /></svg>
    {/if}
  </span>
  <span class="body">
    {#if kind === 'files'}
      {#if item.thumb}
        <img
          class="thumb"
          src={item.thumb}
          alt={item.files[0]?.name}
          draggable="false"
          style:--ratio={item.width && item.height ? item.width / item.height : 16 / 9}
        />
      {/if}
      <span class="text" class:gone>{filesLabel(item.files)}</span>
    {:else if kind === 'image'}
      <!-- Картинка в списке крупно: по ней и узнают, что копировали. -->
      {#if item.thumb}
        <img
          class="thumb"
          src={item.thumb}
          alt="Картинка {item.width}×{item.height}"
          draggable="false"
          style:--ratio={item.width && item.height ? item.width / item.height : 16 / 9}
        />
      {:else}
        <span class="thumb empty">Картинка</span>
      {/if}
    {:else}
      <span class="text" class:mono={kind === 'code'}>{item.text}</span>
    {/if}
    <span class="meta" class:failed={!!progress?.error}
      >{device}{#if item.source_app}&nbsp;· <AppTag app={item.source_app} />{/if} · {meta}</span
    >
    {#if progress}
      <span
        class="bar"
        class:failed={!!progress.error}
        role="progressbar"
        aria-label={progressLabel(progress)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={progress.percent}
      >
        <span style:width="{progress.percent}%"></span>
      </span>
    {/if}
  </span>
  <span class="side">
    {#if index < 9}<kbd class="num">{index + 1}</kbd>{/if}
    <span class="acts">
    {#if onremove}
      <button
        class="pin remove"
        title="Удалить из истории (Delete)"
        aria-label="Удалить из истории"
        onclick={(e) => {
          e.stopPropagation()
          onremove?.()
        }}
        ondblclick={(e) => e.stopPropagation()}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><path d="M5 7h14M10 7V5h4v2M7 7l1 12h8l1-12M10.5 11v5M13.5 11v5" /></svg
        >
      </button>
    {/if}
    {#if onpin || item.pinned}
      <button
        class="pin"
        class:on={item.pinned}
        title={item.pinned ? 'Открепить (Ctrl+P)' : 'Закрепить (Ctrl+P)'}
        aria-label={item.pinned ? 'Открепить' : 'Закрепить'}
        aria-pressed={item.pinned}
        onclick={(e) => {
          e.stopPropagation()
          onpin?.()
        }}
        ondblclick={(e) => e.stopPropagation()}
      >
        <!-- Булавка наклонена, как принято: игла смотрит вниз-влево. -->
        <svg viewBox="0 0 24 24" aria-hidden="true"
          ><g transform="rotate(45 12 12)"
            ><path d="M8.5 3.5h7M10 3.5v5.2L7 12h10l-3-3.3V3.5" /><path d="M12 12v8.5" /></g
          ></svg
        >
      </button>
    {/if}
    </span>
  </span>
</li>

<style>
  li {
    display: grid;
    grid-template-columns: 22px minmax(0, 1fr) auto;
    gap: var(--space-2);
    align-items: start;
    padding: 7px var(--space-2);
    border-radius: var(--radius-m);
    cursor: default;
    position: relative;
  }
  li:hover {
    background: var(--c-hover);
  }
  li.sel {
    background: var(--c-sel);
    box-shadow: inset 0 0 0 1px color-mix(in oklch, var(--c-accent) 55%, transparent);
  }
  .glyph svg {
    width: 18px;
    height: 18px;
    margin-top: 1px;
    fill: none;
    stroke: var(--c-muted);
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .sel .glyph svg {
    stroke: var(--c-accent);
  }
  .body {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .text {
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    overflow-wrap: anywhere;
    white-space: pre-line;
  }
  .text.gone {
    text-decoration: line-through;
    color: var(--c-muted);
  }
  .text.mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  /* Рамка ровно по пропорции картинки: не выше 132 px и не шире строки,
   * без полей по бокам у широких скриншотов. */
  .thumb {
    display: block;
    aspect-ratio: var(--ratio);
    width: min(100%, calc(132px * var(--ratio)));
    height: auto;
    object-fit: cover;
    border-radius: var(--radius-s);
    border: 1px solid var(--c-border);
    background: var(--c-surface);
    margin: 2px 0;
  }
  .thumb.empty {
    width: 100%;
    height: 72px;
    display: grid;
    place-items: center;
    color: var(--c-muted);
    font-size: var(--text-s);
  }
  .meta {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .meta.failed {
    color: var(--c-danger);
  }
  /* Тонкая полоса под метой: сколько файлов уже передано. */
  .bar {
    display: block;
    height: 2px;
    margin-top: 1px;
    border-radius: 1px;
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
  @media (prefers-reduced-motion: reduce) {
    .bar > span {
      transition: none;
    }
  }
  .num {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    opacity: 0;
    margin-top: 2px;
  }
  li.sel .num,
  li:hover .num {
    opacity: 1;
  }
  .side {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 4px;
  }
  /* Корзина и булавка в ряд: третьей строкой они раздули бы каждую запись. */
  .acts {
    display: flex;
    gap: 2px;
  }
  .pin {
    width: 22px;
    height: 22px;
    display: grid;
    place-items: center;
    padding: 0;
    border: 0;
    border-radius: var(--radius-s);
    background: transparent;
    color: var(--c-muted);
    cursor: default;
    opacity: 0;
    transition: opacity var(--dur-fast) var(--ease-out);
  }
  .pin svg {
    width: 15px;
    height: 15px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linejoin: round;
    stroke-linecap: round;
  }
  li:hover .pin,
  li.sel .pin,
  .pin:focus-visible {
    opacity: 1;
  }
  .pin:hover {
    background: var(--c-hover);
    color: var(--c-text);
  }
  .remove:hover {
    color: var(--c-danger);
  }
  /* Закреплённое видно всегда: булавка залита акцентом. */
  .pin.on {
    opacity: 1;
    color: var(--c-accent);
  }
  .pin.on svg path:first-child {
    fill: currentColor;
  }
  @media (prefers-reduced-motion: reduce) {
    .pin {
      transition: none;
    }
  }
</style>
