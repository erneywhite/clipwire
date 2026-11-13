<script lang="ts">
  import { onMount, tick } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type Entry, type FileInfo, type Settings, type Transfer } from '../lib/api'
  import { applyTheme } from '../lib/theme'
  import { ago, bytesLabel, filesMeta, isUrl, looksLikeCode, richDoc, sizeLabel } from '../lib/format'
  import { canDownload, entryProgress, fetchLabel, isGone, isHere, onlyThere, progressLabel, type TransferMap } from '../lib/transfer'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import EntryRow from '../lib/EntryRow.svelte'
  import LinkCard from './LinkCard.svelte'
  import FilePreview from './FilePreview.svelte'
  import { openUrl, revealItemInDir } from '@tauri-apps/plugin-opener'

  let query = $state('')
  let tab = $state<'all' | 'pinned'>('all')
  let items = $state<Entry[]>([])
  let sel = $state(0)
  let preview = $state<{ entry: Entry; side: 'left' | 'right' } | null>(null)
  // Текст в предпросмотре можно поправить перед вставкой.
  let draft = $state('')
  let editor: HTMLTextAreaElement | undefined = $state()
  // Форматированный текст в предпросмотре: как выглядит (true) или сам текст для правки.
  let richView = $state(true)
  const edited = $derived(preview !== null && draft !== preview.entry.text)
  let error = $state('')
  let input: HTMLInputElement | undefined = $state()
  let listEl: HTMLUListElement | undefined = $state()

  const current = $derived(items[sel])

  // Передачи файлов по blob: начальное состояние из transfers(), дальше события.
  let transfers = $state<TransferMap>({})
  // Запись, файлы которой сейчас качаются перед вставкой или по кнопке «Скачать».
  let fetching = $state<string | null>(null)
  let pasting = $state(false)
  // Сколько раз окно показывали. Скачивание перед вставкой может идти долго:
  // если за это время окно открыли заново, начатая вставка уже не нужна.
  let shown = 0
  // Текущая ошибка — от неудачного load(): её снимает следующий удачный.
  let loadFailed = false

  /** Имя устройства, откуда пришла запись. */
  function deviceOf(e: Entry): string {
    return e.local ? 'Это устройство' : e.device_name || 'Другое устройство'
  }

  /**
   * «Показать в папке»: файл выделяется в своей папке, а папка открывается
   * сама: у скачанной папки родитель — служебный кэш, смотреть там нечего.
   */
  function showInFolder(f: FileInfo) {
    const p = f.local_path
    if (!p) return
    const done = f.is_dir ? api.folderOpen(p) : revealItemInDir(p)
    done.catch((e) => (error = String(e)))
  }

  /** Есть файлы с другого устройства, которые надо сначала скачать. */
  function needsDownload(e: Entry | undefined): boolean {
    return !!e && e.kind === 'files' && e.files.some(canDownload)
  }

  /** Скачать файлы записи. true — всё скачалось. */
  async function download(entry: Entry): Promise<boolean> {
    if (fetching) return false
    const session = shown
    fetching = entry.id
    error = ''
    let failure = ''
    try {
      await api.filesDownload(entry.id)
    } catch (e) {
      failure = String(e)
    }
    // Окно открыли заново — там уже своё состояние, его не трогаем.
    if (session !== shown) return false
    fetching = null
    // Пути в кэше появились — обновить строку и предпросмотр. Ошибку ставим
    // после: обновление списка её не стирает, но и не должно её опередить.
    await load()
    await refreshPreview()
    if (failure && session === shown) error = failure
    return !failure
  }

  /** Окно на экране и в фокусе: пользователь всё ещё ждёт вставку. */
  async function stillShown(): Promise<boolean> {
    try {
      const w = getCurrentWindow()
      return (await w.isVisible()) && (await w.isFocused())
    } catch {
      // Не узнали — решит Rust, он проверяет то же самое перед Ctrl+V.
      return true
    }
  }

  async function refreshPreview() {
    if (!preview) return
    const id = preview.entry.id
    try {
      const entry = await api.get(id)
      if (preview?.entry.id === id) preview = { ...preview, entry }
    } catch {
      // Запись успели удалить — предпросмотр закроет следующий load.
    }
  }

  function onTransfer(t: Transfer) {
    transfers[t.blob] = t
    // Скачивание по кнопке ждёт download(); здесь — чужие скачивания, например
    // начатые из другого окна, чтобы предпросмотр не остался на карточке.
    if (t.direction === 'down' && t.state === 'done' && !fetching) {
      load()
      refreshPreview()
    }
  }

  // Чужую ошибку (скачивания, вставки) load() не стирает: её сбрасывают явные
  // действия — новый запрос, смена вкладки, новое открытие окна, новая вставка.
  async function load() {
    try {
      items = await api.list(query, tab === 'pinned')
      if (sel >= items.length) sel = Math.max(0, items.length - 1)
      if (loadFailed) {
        loadFailed = false
        error = ''
      }
    } catch (e) {
      error = String(e)
      loadFailed = true
    }
  }

  /** Явная смена списка: новый запрос или вкладка. Старая ошибка уже ни к чему. */
  function reload() {
    sel = 0
    error = ''
    loadFailed = false
    load()
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

  // Какие файлы записи вставлять: null — все. Сбрасывается при открытии предпросмотра.
  let picked = $state<number[] | null>(null)

  function togglePick(i: number, total: number) {
    const cur = picked ?? Array.from({ length: total }, (_, k) => k)
    const next = cur.includes(i) ? cur.filter((k) => k !== i) : [...cur, i].sort((a, b) => a - b)
    picked = next.length === total ? null : next
  }

  /** Вставить только отмеченные файлы записи. */
  function pastePicked(entry: Entry) {
    if (picked && picked.length === 0) return
    paste(entry, undefined, undefined, picked ?? undefined)
  }

  async function paste(entry: Entry | undefined, intoActive?: boolean, plain?: boolean, only?: number[]) {
    if (!entry || pasting) return
    // Файлы с другого устройства сначала скачиваются: окно остаётся открытым и
    // показывает прогресс. popup_paste скачал бы и сам, но тогда до конца
    // загрузки не видно, что происходит, а ошибка приходит уже после закрытия.
    // Часть файлов Rust скачает сам: заранее качать все незачем.
    if (!only && needsDownload(entry)) {
      // shortcut: одно скачивание за раз, второе не встаёт в очередь; переделать, если будут жалобы.
      if (fetching) {
        error = fetching === entry.id ? 'Файлы ещё скачиваются' : 'Сначала дождитесь, пока скачаются файлы другой записи'
        return
      }
      const session = shown
      pasting = true
      const ok = await download(entry)
      // Пока качалось, окно открыли заново (там уже сброшено своё состояние)
      // или спрятали: Ctrl+V ушёл бы туда, где пользователь уже работает.
      // Файлы остаются в кэше, вставить можно ещё раз.
      if (session !== shown) return
      pasting = false
      if (!ok || !(await stillShown()) || session !== shown) return
    } else {
      error = ''
    }
    // Если это элемент из предпросмотра и его поправили — вставляем изменённый текст.
    const text = preview && preview.entry.id === entry.id && edited ? draft : null
    await closePreview()
    try {
      if (text !== null) await api.pasteText(text, intoActive)
      else await api.paste(entry.id, intoActive, plain, only)
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
    if (preview?.entry.id !== entry.id) picked = null
    preview = { entry, side }
    draft = entry.text
    richView = true
  }

  async function closePreview() {
    if (!preview) return
    preview = null
    await api.preview(false)
  }

  /** Картинка или файл-картинка — их можно открыть в редакторе. */
  function editable(e: Entry | undefined): boolean {
    return !!e && (e.kind === 'image' || (e.kind === 'files' && e.width > 0))
  }

  /** Открыть картинку в редакторе: стрелки, текст, размытие. */
  async function edit(entry: Entry | undefined) {
    if (!entry || !editable(entry)) return
    await closePreview()
    try {
      await api.editImage(entry.id)
    } catch (e) {
      error = String(e)
    }
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
        // Shift+Enter — без форматирования.
        e.preventDefault()
        // В предпросмотре файлов отмечена часть — вставляем только её.
        if (picked && current && preview?.entry.id === current.id) pastePicked(current)
        else paste(current, undefined, e.shiftKey ? true : undefined)
        return
      case 'Escape':
        e.preventDefault()
        if (preview) await closePreview()
        else await api.hidePopup()
        return
      case 'Tab':
        e.preventDefault()
        tab = tab === 'all' ? 'pinned' : 'all'
        reload()
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
    if ((e.ctrlKey || e.metaKey) && e.code === 'KeyE') {
      e.preventDefault()
      edit(current)
    } else if ((e.ctrlKey || e.metaKey) && e.code === 'KeyP') {
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
      listen<Transfer>('clipwire://transfer', (e) => onTransfer(e.payload)),
      listen('clipwire://history-changed', () => load()),
      listen<Settings>('clipwire://settings-changed', (e) => applySettings(e.payload)),
      // Живое превью размера из настроек: ширина списка меняется вместе с окном.
      listen('popup://opened', async () => {
        // Новый показ: начатые в прошлый раз скачивание и вставка сюда уже не
        // относятся (скачивание дойдёт в фоне, но вставлять не станет).
        shown++
        pasting = false
        fetching = null
        query = ''
        sel = 0
        preview = null
        error = ''
        loadFailed = false
        await load()
        await tick()
        // Окно не пересоздаётся, а прокрутка остаётся с прошлого раза: без этого
        // список мог открыться в середине, и свежие элементы были не видны.
        if (listEl) listEl.scrollTop = 0
        input?.focus()
      }),
    ]
    // Начальные передачи — после подписки, и событие новее не затирается.
    unlisten[0]
      .then(() => api.transfers())
      .then((list) => {
        for (const t of list) if (!transfers[t.blob]) transfers[t.blob] = t
      })
      .catch(() => {})
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
        oninput={reload}
        placeholder="Найти в буфере"
        spellcheck="false"
        aria-label="Поиск по истории"
      />
    </label>

    <div class="tabs" role="tablist">
      <button role="tab" aria-selected={tab === 'all'} onclick={() => { tab = 'all'; reload() }}>История</button>
      <button role="tab" aria-selected={tab === 'pinned'} onclick={() => { tab = 'pinned'; reload() }}>Закреплённое</button>
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
          <EntryRow {item} {transfers} index={i} selected={i === sel} onselect={() => (sel = i)} onpaste={() => paste(item)} />
        {/each}
      </ul>
    {/if}

    <footer class="hints">
      <span><kbd>↵</kbd> вставить</span>
      <span><kbd>␣</kbd> просмотр</span>
      {#if editable(current)}
        <span><kbd>Ctrl E</kbd> изменить</span>
      {:else}
        <span><kbd>Ctrl P</kbd> закрепить</span>
      {/if}
      <span><kbd>Esc</kbd></span>
    </footer>
  </section>

  {#if preview}
    <section class="preview-pane" aria-label="Предпросмотр">
      {#if preview.entry.kind === 'image'}
        <div class="image-view">
          {#if preview.entry.image}
            <img src={preview.entry.image} alt="Картинка {preview.entry.width}×{preview.entry.height}" draggable="false" />
          {/if}
        </div>
        <div class="preview-meta">
          {preview.entry.local ? 'Это устройство' : preview.entry.device_name} · {ago(preview.entry.seen_ms)} · {preview.entry.width}×{preview.entry.height} · {bytesLabel(preview.entry.bytes)}
        </div>
        <div class="actions">
          <button class="primary" onclick={() => paste(preview?.entry)}>Вставить</button>
          <button onclick={() => edit(preview?.entry)}>Изменить</button>
          <button onclick={() => togglePin(preview?.entry).then(openPreview)}>
            {preview.entry.pinned ? 'Открепить' : 'Закрепить'}
          </button>
        </div>
      {:else if preview.entry.kind === 'files'}
        {@const entry = preview.entry}
        {@const device = deviceOf(entry)}
        {@const single = !entry.image && entry.files.length === 1}
        {@const progress = entryProgress(entry.files, transfers)}
        {#if entry.image}
          <div class="image-view">
            <img src={entry.image} alt={entry.files[0]?.name} draggable="false" />
          </div>
        {:else if single}
          <FilePreview file={entry.files[0]} {device} {transfers} ondownload={() => download(entry)} />
        {:else}
          <ul class="file-list">
            {#each entry.files as f, i (f.path)}
              {@const fp = entryProgress([f], transfers)}
              <li class:gone={isGone(f)} class:off={picked !== null && !picked.includes(i)}>
                <label class="fname">
                  <input
                    type="checkbox"
                    checked={picked === null || picked.includes(i)}
                    onchange={() => togglePick(i, entry.files.length)}
                    aria-label="Вставить «{f.name}»"
                  />
                  <span>{f.name}</span>
                </label>
                <span class="fmeta">
                  {f.is_dir ? (f.size > 0 ? `папка · ${bytesLabel(f.size)}` : 'папка') : bytesLabel(f.size)}{#if isGone(f)}
                    · {f.is_dir ? 'папки' : 'файла'} нет на месте{:else if onlyThere(f)}
                    · есть только на устройстве «{device}»{:else if fp}
                    · {progressLabel(fp)}{:else if canDownload(f)}
                    · {fetchLabel(f, device)}{/if}
                </span>
                {#if isHere(f)}
                  <span class="fpath">{f.local_path}</span>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
        <div class="preview-meta">
          {entry.files.length === 1 ? `${entry.files[0].name} · ` : ''}{filesMeta(entry.files, entry.bytes, entry.width, entry.height)}{#if !single && progress}
            · {progressLabel(progress)}{:else if entry.image && onlyThere(entry.files[0])}
            · есть только на устройстве «{device}»{/if}
        </div>
        <div class="actions">
          <button class="primary" disabled={pasting || picked?.length === 0} onclick={() => pastePicked(entry)}>
            {#if pasting && fetching === entry.id}
              Скачивается{progress && !progress.error ? ` ${progress.percent}%` : '…'}
            {:else if picked}
              Вставить выбранные ({picked.length})
            {:else}
              Вставить
            {/if}
          </button>
          {#if editable(entry)}
            <button onclick={() => edit(entry)}>Изменить</button>
          {/if}
          {#if !single && entry.files.some(canDownload) && !pasting}
            <button disabled={fetching === entry.id} onclick={() => download(entry)}>
              {fetching === entry.id ? 'Скачивается…' : 'Скачать'}
            </button>
          {/if}
          {#if isHere(entry.files[0])}
            <button onclick={() => showInFolder(entry.files[0])}>Показать в папке</button>
          {/if}
          <button onclick={() => togglePin(entry).then(openPreview)}>
            {entry.pinned ? 'Открепить' : 'Закрепить'}
          </button>
        </div>
      {:else}
      {#if preview.entry.html || isUrl(preview.entry.text)}
        <div class="seg" role="tablist" aria-label="Вид">
          <button role="tab" aria-selected={richView} onclick={() => (richView = true)}>
            {isUrl(preview.entry.text) ? 'Карточка' : 'Форматирование'}
          </button>
          <button role="tab" aria-selected={!richView} onclick={() => (richView = false)}>Текст</button>
        </div>
      {/if}
      {#if isUrl(preview.entry.text) && richView}
        <LinkCard url={preview.entry.text.trim()} />
      {:else if preview.entry.html && richView}
        <iframe class="rich-view" sandbox="" title="Текст с форматированием" srcdoc={richDoc(preview.entry.html)}></iframe>
      {:else}
        <textarea
          bind:this={editor}
          bind:value={draft}
          class="editor"
          class:mono={looksLikeCode(draft)}
          spellcheck="false"
          aria-label="Текст элемента, можно поправить перед вставкой"
        ></textarea>
      {/if}
      <div class="preview-meta">
        {#if edited}
          изменено · Ctrl+Enter вставить · Ctrl+S сохранить
        {:else}
          {preview.entry.local ? 'Это устройство' : preview.entry.device_name} · {ago(preview.entry.seen_ms)} · {sizeLabel(preview.entry.chars, preview.entry.lines)}{preview.entry.rich ? ` · ${preview.entry.rich}` : ''}
        {/if}
      </div>
      <div class="actions">
        <button class="primary" onclick={() => paste(preview?.entry)}>Вставить</button>
        {#if edited}
          <button onclick={saveDraft}>Сохранить в истории</button>
          <button onclick={() => preview && (draft = preview.entry.text)}>Отменить правку</button>
        {:else}
          {#if isUrl(preview.entry.text)}
            <button onclick={() => preview && openUrl(preview.entry.text.trim())}>Открыть в браузере</button>
          {:else if preview.entry.rich}
            <button onclick={() => paste(preview?.entry, undefined, true)}>Без форматирования</button>
          {/if}
          <button onclick={() => togglePin(preview?.entry).then(openPreview)}>
            {preview.entry.pinned ? 'Открепить' : 'Закрепить'}
          </button>
        {/if}
      </div>
      {/if}
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
  .preview-meta {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .editor.mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
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
  .seg {
    display: flex;
    gap: 2px;
    padding: 2px;
    align-self: flex-start;
    border-radius: var(--radius-m);
    background: var(--c-bg);
  }
  .seg button {
    border: 0;
    background: transparent;
    color: var(--c-muted);
    font: inherit;
    font-size: var(--text-xs);
    padding: 3px var(--space-2);
    border-radius: var(--radius-s);
    cursor: default;
  }
  .seg button[aria-selected='true'] {
    background: var(--c-surface);
    color: var(--c-text);
    box-shadow: 0 0 0 1px var(--c-border);
  }
  /* Бумага, а не тема: HTML из писем и страниц почти всегда свёрстан под белый фон. */
  .rich-view {
    flex: 1;
    min-height: 0;
    width: 100%;
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    background: #fff;
  }
  .file-list {
    flex: 1;
    min-height: 0;
    overflow: auto;
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .file-list li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 0 var(--space-2);
    padding: var(--space-2);
    border-radius: var(--radius-m);
    background: var(--c-bg);
  }
  .fname {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fmeta,
  .fpath {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
  }
  .fpath {
    grid-column: 1 / -1;
    overflow-wrap: anywhere;
  }
  .fname {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .fname span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fname input {
    flex: none;
    margin: 0;
    accent-color: var(--c-accent);
  }
  .file-list li.off {
    opacity: 0.55;
  }
  .file-list li.gone .fname {
    text-decoration: line-through;
    color: var(--c-muted);
  }
  .image-view {
    flex: 1;
    min-height: 0;
    display: grid;
    place-items: center;
    background: var(--c-bg);
    border-radius: var(--radius-m);
    overflow: hidden;
  }
  .image-view img {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
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
  .actions button:disabled {
    opacity: 0.7;
    cursor: default;
  }
  .actions button:active:not(:disabled) {
    transform: translateY(1px);
  }
</style>
