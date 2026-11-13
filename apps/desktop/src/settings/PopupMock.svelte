<script lang="ts">
  // Окно вставки на примерах — для превью размера в настройках. Показывается
  // в отдельном окне рядом с настройками, которое не забирает фокус.
  import EntryRow from '../lib/EntryRow.svelte'
  import type { Entry } from '../lib/api'

  // fill — занять всё окно (отдельное окно-превью); иначе заданный размер.
  let { width = 0, height = 0, fill = false }: { width?: number; height?: number; fill?: boolean } = $props()

  const now = Date.now()
  const min = 60_000
  const base = { local: true, pinned: false, device_name: '', lines: 1, width: 0, height: 0, bytes: 0, thumb: null, image: null, rich: null, html: null }
  // Образец картинки: схематичный скриншот окна (16:9), чтобы превью показывало
  // настоящую высоту строки с картинкой.
  const sampleShot =
    'data:image/svg+xml;utf8,' +
    encodeURIComponent(
      `<svg xmlns="http://www.w3.org/2000/svg" width="320" height="180" viewBox="0 0 320 180">
        <rect width="320" height="180" fill="#2b2f36"/>
        <rect width="320" height="22" fill="#3a3f48"/>
        <circle cx="12" cy="11" r="4" fill="#e5484d"/><circle cx="26" cy="11" r="4" fill="#f0b030"/><circle cx="40" cy="11" r="4" fill="#3fbf7a"/>
        <rect x="14" y="36" width="90" height="130" rx="4" fill="#353a43"/>
        <rect x="118" y="36" width="188" height="14" rx="3" fill="#e2a63b"/>
        <rect x="118" y="60" width="160" height="8" rx="3" fill="#596070"/>
        <rect x="118" y="76" width="176" height="8" rx="3" fill="#596070"/>
        <rect x="118" y="92" width="120" height="8" rx="3" fill="#596070"/>
        <rect x="118" y="114" width="188" height="52" rx="4" fill="#3d4450"/>
      </svg>`,
    )
  const samples: Entry[] = [
    { ...base, id: '1', kind: 'image', text: '', chars: 0, seen_ms: now - 1 * min, local: false, device_name: 'MacBook', width: 1920, height: 1080, bytes: 348_000, thumb: sampleShot },
    { ...base, id: '2', kind: 'text', text: 'https://github.com/erneywhite/clipwire', chars: 38, seen_ms: now - 4 * min },
    { ...base, id: '3', kind: 'text', text: 'docker compose pull && docker compose up -d', chars: 43, seen_ms: now - 9 * min },
    { ...base, id: '4', kind: 'text', text: 'Созвон завтра в 10:00, ссылку пришлю утром', chars: 42, seen_ms: now - 25 * min, local: false, device_name: 'MacBook', pinned: true },
    { ...base, id: '5', kind: 'text', text: 'Ответ клиенту: заказ отправлен, номер для отслеживания пришлю отдельным письмом сегодня вечером', chars: 96, lines: 1, seen_ms: now - 50 * min },
    { ...base, id: '6', kind: 'text', text: 'SELECT id, name\nFROM devices\nWHERE last_seen > now() - interval 1 day;', chars: 70, lines: 3, seen_ms: now - 80 * min },
    { ...base, id: '7', kind: 'text', text: 'Адрес доставки: Рига, Бривибас 1', chars: 32, seen_ms: now - 130 * min },
  ]
</script>

<div class="frame" style:width={fill ? '100vw' : `${width}px`} style:height={fill ? '100vh' : `${height}px`} aria-hidden="true">
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
