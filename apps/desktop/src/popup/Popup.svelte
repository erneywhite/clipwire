<script lang="ts">
  import { onMount, tick } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type Entry, type Settings } from '../lib/api'
  import { applyTheme } from '../lib/theme'
  import { ago, isUrl, looksLikeCode, sizeLabel } from '../lib/format'

  let query = $state('')
  let tab = $state<'all' | 'pinned'>('all')
  let items = $state<Entry[]>([])
  let sel = $state(0)
  let preview = $state<{ entry: Entry; side: 'left' | 'right' } | null>(null)
  // Текст в предпросмотре можно поправить перед вставкой.
  let draft = $state('')
  let editor: HTMLTextAreaElement | undefined = $state()
  const edited = $derived(preview !== null && draft !== preview.entry.text)
  let error = $state('')
  let input: HTMLInputElement | undefined = $state()
  let listEl: HTMLUListElement | undefined = $state()

  const current = $derived(items[sel])

  async function load() {
    try {
      items = await api.list(query, tab === 'pinned')
      if (sel >= items.length) sel = Math.max(0, items.length - 1)
      error = ''
    } catch (e) {
      error = String(e)
    }
  }

  async function scrollToSel() {
    await tick()
    listEl?.querySelector<HTMLElement>(`[data-i="${sel}"]`)?.scrollIntoView({ block: 'nearest' })
  }

  function move(d: number) {
    if (!items.length) return
    sel = (sel + d + items.length) % items.length
    scrollToSel()
    if (preview) openPreview()
  }

  async function paste(entry: Entry | undefined, intoActive?: boolean) {
    if (!entry) return
    // Если это элемент из предпросмотра и его поправили — вставляем изменённый текст.
    const text = preview && preview.entry.id === entry.id && edited ? draft : null
    await closePreview()
    try {
      if (text !== null) await api.pasteText(text, intoActive)
      else await api.paste(entry.id, intoActive)
    } catch (e) {
      error = String(e)
    }
  }

  async function saveDraft() {
    if (!preview || !edited) return
    try {
      await api.update(preview.entry.id, draft)
      preview = { ...preview, entry: await api.get(preview.entry.id) }
      await load()
    } catch (e) {
      error = String(e)
    }
  }

  async function openPreview() {
    if (!current) return
    const entry = await api.get(current.id)
    const side = preview ? preview.side : await api.preview(true)
    preview = { entry, side }
    draft = entry.text
  }

  async function closePreview() {
    if (!preview) return
    preview = null
    await api.preview(false)
  }

  async function togglePin(entry: Entry | undefined) {
    if (!entry) return
    await api.pin(entry.id, !entry.pinned)
    await load()
  }

  async function remove(entry: Entry | undefined) {
    if (!entry) return
    await api.remove(entry.id)
    await load()
  }

  async function onKey(e: KeyboardEvent) {
    // В редакторе предпросмотра клавиши работают как в обычном поле ввода,
    // кроме двух: Ctrl+Enter вставляет изменённый текст, Esc возвращает к списку.
    if (e.target === editor) {
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault()
        paste(preview?.entry)
      } else if (e.key === 'Escape') {
        e.preventDefault()
        editor?.blur()
        input?.focus()
      } else if ((e.ctrlKey || e.metaKey) && e.code === 'KeyS') {
        e.preventDefault()
        saveDraft()
      }
      return
    }
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault()
        move(1)
        return
      case 'ArrowUp':
        e.preventDefault()
        move(-1)
        return
      case 'Enter':
        e.preventDefault()
        paste(current)
        return
      case 'Escape':
        e.preventDefault()
        if (preview) await closePreview()
        else await api.hidePopup()
        return
      case 'Tab':
        e.preventDefault()
        tab = tab === 'all' ? 'pinned' : 'all'
        sel = 0
        load()
        return
      case ' ':
        // Пробел открывает предпросмотр, пока строка поиска пустая; иначе это пробел в запросе.
        if (query === '') {
          e.preventDefault()
          if (preview) await closePreview()
          else await openPreview()
        }
        return
    }
    if ((e.ctrlKey || e.metaKey) && e.code === 'KeyP') {
      e.preventDefault()
      togglePin(current)
    } else if (e.shiftKey && e.key === 'Delete') {
      e.preventDefault()
      remove(current)
    } else if ((e.ctrlKey || e.metaKey) && /^Digit[1-9]$/.test(e.code)) {
      e.preventDefault()
      paste(items[Number(e.code.slice(5)) - 1])
    }
  }

  function applySettings(s: Settings) {
    applyTheme(s.theme, s.mode)
    // При открытом предпросмотре окно шире, а список сохраняет свою ширину.
    document.documentElement.style.setProperty('--popup-w', `${s.popup_width}px`)
  }

  onMount(() => {
    api.settings().then(applySettings)
    load()
    const unlisten = [
      listen('clipwire://history-changed', () => load()),
      listen<Settings>('clipwire://settings-changed', (e) => applySettings(e.payload)),
      // Живое превью размера из настроек: ширина списка меняется вместе с окном.
      // Живое превью размера: только ширина списка, без перезагрузки — это вызывается часто.
      listen<number>('popup://demo-size', (e) => {
        document.documentElement.style.setProperty('--popup-w', `${e.payload}px`)
      }),
      listen('popup://opened', async () => {
        query = ''
        sel = 0
        preview = null
        error = ''
        await load()
        await tick()
        input?.focus()
      }),
    ]
    return () => unlisten.forEach((p) => p.then((f) => f()))
  })
</script>

<svelte:window onkeydown={onKey} />

<div class="frame" class:left={preview?.side === 'left'}>
  <section class="list-pane">
    <label class="search">
      <svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="6.5" /><path d="m16 16 4.5 4.5" /></svg>
      <input
        bind:this={input}
        bind:value={query}
        oninput={() => {
          sel = 0
          load()
        }}
        placeholder="Найти в буфере"
        spellcheck="false"
        aria-label="Поиск по истории"
      />
    </label>

    <div class="tabs" role="tablist">
      <button role="tab" aria-selected={tab === 'all'} onclick={() => { tab = 'all'; sel = 0; load() }}>История</button>
      <button role="tab" aria-selected={tab === 'pinned'} onclick={() => { tab = 'pinned'; sel = 0; load() }}>Закреплённое</button>
    </div>

    {#if error}
      <p class="error">{error}</p>
    {/if}

    {#if items.length === 0}
      <div class="empty">
        {#if query}
          <p>Ничего не нашлось по «{query}»</p>
        {:else if tab === 'pinned'}
          <p>Закрепите нужное сочетанием Ctrl+P, и оно не пропадёт из истории</p>
        {:else}
          <p>Здесь появится всё, что копируется на ваших устройствах</p>
        {/if}
      </div>
    {:else}
      <ul class="list" bind:this={listEl} role="listbox" aria-label="История буфера">
        {#each items as item, i (item.id)}
          <li
            data-i={i}
            role="option"
            aria-selected={i === sel}
            class:sel={i === sel}
            onclick={() => (sel = i)}
            ondblclick={() => paste(item)}
            onkeydown={() => {}}
          >
            <span class="glyph" aria-hidden="true">
              {#if isUrl(item.text)}
                <svg viewBox="0 0 24 24"><path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" /><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" /></svg>
              {:else if looksLikeCode(item.text)}
                <svg viewBox="0 0 24 24"><path d="m8 8-4 4 4 4M16 8l4 4-4 4" /></svg>
              {:else}
                <svg viewBox="0 0 24 24"><path d="M5 7h14M5 12h14M5 17h9" /></svg>
              {/if}
            </span>
            <span class="body">
              <span class="text" class:mono={looksLikeCode(item.text)}>{item.text}</span>
              <span class="meta">
                {item.local ? 'Это устройство' : item.device_name || 'Другое устройство'} · {ago(item.seen_ms)} · {sizeLabel(item.chars, item.lines)}
              </span>
            </span>
            {#if i < 9}<kbd class="num">{i + 1}</kbd>{/if}
            {#if item.pinned}<span class="pin" aria-label="Закреплено">●</span>{/if}
          </li>
        {/each}
      </ul>
    {/if}

    <footer class="hints">
      <span><kbd>↵</kbd> вставить</span>
      <span><kbd>␣</kbd> просмотр</span>
      <span><kbd>Ctrl P</kbd> закрепить</span>
      <span><kbd>Esc</kbd></span>
    </footer>
  </section>

  {#if preview}
    <section class="preview-pane" aria-label="Предпросмотр">
      {#if isUrl(preview.entry.text)}
        <p class="note">Карточки ссылок появятся в следующей версии</p>
      {/if}
      <textarea
        bind:this={editor}
        bind:value={draft}
        class="editor"
        class:mono={looksLikeCode(draft)}
        spellcheck="false"
        aria-label="Текст элемента, можно поправить перед вставкой"
      ></textarea>
      <div class="preview-meta">
        {#if edited}
          изменено · Ctrl+Enter вставить · Ctrl+S сохранить
        {:else}
          {preview.entry.local ? 'Это устройство' : preview.entry.device_name} · {ago(preview.entry.seen_ms)} · {sizeLabel(preview.entry.chars, preview.entry.lines)}
        {/if}
      </div>
      <div class="actions">
        <button class="primary" onclick={() => paste(preview?.entry)}>Вставить</button>
        {#if edited}
          <button onclick={saveDraft}>Сохранить в истории</button>
          <button onclick={() => preview && (draft = preview.entry.text)}>Отменить правку</button>
        {:else}
          <button onclick={() => togglePin(preview?.entry).then(openPreview)}>
            {preview.entry.pinned ? 'Открепить' : 'Закрепить'}
          </button>
        {/if}
      </div>
    </section>
  {/if}
</div>

<style>
  .frame {
    display: flex;
    height: 100vh;
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-l);
    overflow: hidden;
  }
  .frame.left {
    flex-direction: row-reverse;
  }
  .list-pane {
    flex: 0 0 auto;
    width: min(100%, var(--popup-w, 380px));
    min-width: 0;
    display: flex;
    flex-direction: column;
    padding: var(--space-2);
  }
  .frame:not(:has(.preview-pane)) .list-pane {
    width: 100%;
  }
  .search {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 0 var(--space-3);
    color: var(--c-muted);
  }
  .search svg {
    width: 16px;
    height: 16px;
    flex: none;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
  }
  .search input {
    flex: 1;
    min-width: 0;
    border: 0;
    background: transparent;
    padding: 9px 0;
    outline: none;
    color: var(--c-text);
  }
  .search input::placeholder {
    color: var(--c-muted);
  }
  .tabs {
    display: flex;
    gap: var(--space-4);
    padding: var(--space-3) var(--space-1) var(--space-2);
  }
  .tabs button {
    border: 0;
    background: none;
    padding: 0 0 3px;
    font-size: var(--text-s);
    color: var(--c-muted);
    border-bottom: 2px solid transparent;
    cursor: pointer;
    white-space: nowrap;
  }
  .tabs button[aria-selected='true'] {
    color: var(--c-text);
    border-bottom-color: var(--c-accent);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    flex: 1;
  }
  .list li {
    display: grid;
    grid-template-columns: 22px minmax(0, 1fr) auto;
    gap: var(--space-2);
    align-items: start;
    padding: 7px var(--space-2);
    border-radius: var(--radius-m);
    cursor: default;
    position: relative;
  }
  .list li:hover {
    background: var(--c-hover);
  }
  .list li.sel {
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
  .text.mono,
  .editor.mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .meta,
  .preview-meta {
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
  .list li.sel .num,
  .list li:hover .num {
    opacity: 1;
  }
  .pin {
    position: absolute;
    left: 4px;
    top: 6px;
    font-size: 7px;
    color: var(--c-accent);
  }
  .empty {
    flex: 1;
    display: grid;
    place-items: center;
    text-align: center;
    color: var(--c-muted);
    padding: var(--space-5);
  }
  .error {
    color: var(--c-danger);
    font-size: var(--text-s);
    margin: 0 var(--space-1) var(--space-2);
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
  kbd {
    font-family: var(--font-mono);
  }
  .preview-pane {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--c-surface);
    border-left: 1px solid var(--c-border);
  }
  .frame.left .preview-pane {
    border-left: 0;
    border-right: 1px solid var(--c-border);
  }
  .editor {
    flex: 1;
    margin: 0;
    resize: none;
    border: 1px solid transparent;
    border-radius: var(--radius-m);
    background: transparent;
    padding: var(--space-2);
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font-family: var(--font-ui);
    font-size: var(--text-m);
    line-height: 1.45;
    color: var(--c-text);
    outline: none;
  }
  .editor:hover {
    border-color: var(--c-border);
  }
  .editor:focus {
    border-color: var(--c-accent);
    background: var(--c-bg);
  }
  .note {
    margin: 0;
    color: var(--c-muted);
    font-size: var(--text-s);
  }
  .actions {
    display: flex;
    gap: var(--space-2);
  }
  .actions button {
    border: 1px solid var(--c-border);
    background: transparent;
    border-radius: var(--radius-m);
    padding: 6px 12px;
    cursor: pointer;
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .actions button:hover {
    background: var(--c-hover);
  }
  .actions button.primary {
    background: var(--c-accent);
    border-color: var(--c-accent);
    color: var(--c-on-accent);
  }
  .actions button:active {
    transform: translateY(1px);
  }
</style>
