<script lang="ts">
  // Строка элемента истории. Общая для окна вставки и для превью в настройках,
  // чтобы превью выглядело ровно как настоящее окно.
  import type { Entry } from './api'
  import { ago, isUrl, looksLikeCode, sizeLabel } from './format'

  let {
    item,
    index,
    selected = false,
    onselect,
    onpaste,
  }: {
    item: Entry & { thumb?: string; width?: number; height?: number }
    index: number
    selected?: boolean
    onselect?: () => void
    onpaste?: () => void
  } = $props()

  const kind = $derived(item.kind === 'image' ? 'image' : isUrl(item.text) ? 'link' : looksLikeCode(item.text) ? 'code' : 'text')
  const meta = $derived(
    [
      item.local ? 'Это устройство' : item.device_name || 'Другое устройство',
      ago(item.seen_ms),
      kind === 'image' && item.width ? `${item.width}×${item.height}` : sizeLabel(item.chars, item.lines),
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
  onkeydown={() => {}}
>
  <span class="glyph" aria-hidden="true">
    {#if kind === 'image'}
      <svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="14" rx="2" /><path d="m4 16 5-5 4 4 2-2 5 5" /></svg>
    {:else if kind === 'link'}
      <svg viewBox="0 0 24 24"><path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" /><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" /></svg>
    {:else if kind === 'code'}
      <svg viewBox="0 0 24 24"><path d="m8 8-4 4 4 4M16 8l4 4-4 4" /></svg>
    {:else}
      <svg viewBox="0 0 24 24"><path d="M5 7h14M5 12h14M5 17h9" /></svg>
    {/if}
  </span>
  <span class="body">
    {#if kind === 'image'}
      <span class="image-row">
        <span class="thumb" style:background={item.thumb ?? 'var(--c-surface)'}></span>
        <span class="text">{item.text}</span>
      </span>
    {:else}
      <span class="text" class:mono={kind === 'code'}>{item.text}</span>
    {/if}
    <span class="meta">{meta}</span>
  </span>
  {#if index < 9}<kbd class="num">{index + 1}</kbd>{/if}
  {#if item.pinned}<span class="pin" aria-label="Закреплено">●</span>{/if}
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
  .text.mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .image-row {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    min-width: 0;
  }
  .thumb {
    flex: none;
    width: 64px;
    height: 40px;
    border-radius: var(--radius-s);
    border: 1px solid var(--c-border);
  }
  .meta {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
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
  .pin {
    position: absolute;
    left: 4px;
    top: 6px;
    font-size: 7px;
    color: var(--c-accent);
  }
</style>
