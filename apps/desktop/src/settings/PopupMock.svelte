<script lang="ts">
  // Окно вставки на примерах — для превью размера в настройках. Живёт внутри
  // окна настроек: никакого второго окна, фокус и курсор никуда не уходят.
  import EntryRow from '../lib/EntryRow.svelte'
  import type { Entry } from '../lib/api'

  let { width, height }: { width: number; height: number } = $props()

  const now = Date.now()
  const min = 60_000
  const base = { local: true, pinned: false, device_name: '', lines: 1 }
  const samples: (Entry & { thumb?: string; width?: number; height?: number })[] = [
    { ...base, id: '1', kind: 'image', text: 'Снимок экрана', chars: 0, seen_ms: now - 1 * min, local: false, device_name: 'MacBook', width: 1280, height: 720, thumb: 'color-mix(in oklch, var(--c-accent) 35%, var(--c-surface))' },
    { ...base, id: '2', kind: 'text', text: 'https://github.com/erneywhite/clipwire', chars: 38, seen_ms: now - 4 * min },
    { ...base, id: '3', kind: 'text', text: 'docker compose pull && docker compose up -d', chars: 43, seen_ms: now - 9 * min },
    { ...base, id: '4', kind: 'text', text: 'Созвон завтра в 10:00, ссылку пришлю утром', chars: 42, seen_ms: now - 25 * min, local: false, device_name: 'MacBook', pinned: true },
    { ...base, id: '5', kind: 'text', text: 'Ответ клиенту: заказ отправлен, номер для отслеживания пришлю отдельным письмом сегодня вечером', chars: 96, lines: 1, seen_ms: now - 50 * min },
    { ...base, id: '6', kind: 'text', text: 'SELECT id, name\nFROM devices\nWHERE last_seen > now() - interval 1 day;', chars: 70, lines: 3, seen_ms: now - 80 * min },
    { ...base, id: '7', kind: 'text', text: 'Адрес доставки: Рига, Бривибас 1', chars: 32, seen_ms: now - 130 * min },
  ]
</script>

<div class="frame" style:width="{width}px" style:height="{height}px" aria-hidden="true">
  <div class="search">
    <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="6.5" /><path d="m16 16 4.5 4.5" /></svg>
    <span>Найти в буфере</span>
  </div>
  <div class="tabs"><span class="on">История</span><span>Закреплённое</span></div>
  <ul class="list">
    {#each samples as item, i (item.id)}
      <EntryRow {item} index={i} selected={i === 0} />
    {/each}
  </ul>
  <div class="hints"><span>↵ вставить</span><span>␣ просмотр</span><span>Ctrl P закрепить</span><span>Esc</span></div>
</div>

<style>
  .frame {
    display: flex;
    flex-direction: column;
    padding: var(--space-2);
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-l);
    overflow: hidden;
    pointer-events: none;
  }
  .search {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 9px var(--space-3);
    color: var(--c-muted);
  }
  .search svg {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
  }
  .tabs {
    display: flex;
    gap: var(--space-4);
    padding: var(--space-3) var(--space-1) var(--space-2);
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .tabs .on {
    color: var(--c-text);
    border-bottom: 2px solid var(--c-accent);
    padding-bottom: 3px;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    flex: 1;
    overflow: hidden;
  }
  .hints {
    display: flex;
    gap: var(--space-3);
    flex-wrap: wrap;
    padding: var(--space-2) var(--space-1) 0;
    margin-top: var(--space-2);
    border-top: 1px solid var(--c-border);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
  }
</style>
