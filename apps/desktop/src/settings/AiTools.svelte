<script lang="ts">
  // Вкладка «ИИ-инструменты»: доступ ИИ-ассистентов к истории через MCP.
  // Настройки лежат в settings.ai и сохраняются обычным save_settings. Список
  // клиентов и журнал обращений меняются отдельными командами: клиентов
  // добавляет и канал MCP, и копия окна не должна его затирать.
  import { onMount } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type AiAccess, type AiBridgeCommand, type AiClient, type AiLogEntry, type AiSettings, type Entry, type Settings } from '../lib/api'
  import { lang, t } from '../lib/i18n'
  import { ago } from '../lib/format'
  import { bridgeSnippets } from '../lib/ai'
  import Toggle from './Toggle.svelte'

  let { s, save }: { s: Settings; save: (patch: Partial<Settings>) => Promise<void> } = $props()

  const ai = $derived(s.ai)

  function saveAi(patch: Partial<AiSettings>) {
    return save({ ai: { ...s.ai, ...patch } })
  }

  // ── Подключение ──
  let bridge = $state<AiBridgeCommand | null>(null)
  let bridgeError = $state('')
  let copied = $state('')
  const snippets = $derived(bridge ? bridgeSnippets(bridge) : [])

  async function copy(id: string, text: string) {
    try {
      // Мимо истории: путь к программе в истории буфера не нужен.
      await api.copyQuietly(text)
      copied = id
      setTimeout(() => (copied === id ? (copied = '') : null), 2000)
    } catch (e) {
      bridgeError = String(e)
    }
  }

  // ── Скрывать похожее на пароли ──
  /** Просят выключить скрытие секретов: сначала спросить. */
  let confirmSecrets = $state(false)
  let keepButton = $state<HTMLButtonElement | null>(null)
  $effect(() => {
    if (confirmSecrets && keepButton) keepButton.focus()
  })

  // ── Срок ──
  const windows: { days: number; title: string }[] = [
    { days: 1, title: t('1 день') },
    { days: 7, title: t('7 дней') },
    { days: 30, title: t('30 дней') },
    { days: 0, title: t('всю историю') },
  ]

  // ── Клиенты ──
  const accesses: { id: AiAccess; title: string }[] = [
    { id: 'denied', title: t('Нет доступа') },
    { id: 'read', title: t('Чтение') },
    { id: 'read_write', title: t('Чтение и запись') },
  ]
  const clients = $derived([...ai.clients].sort((a, b) => b.last_ms - a.last_ms))

  // Новый список клиентов приходит событием clipwire://settings-changed.
  let clientError = $state('')

  async function setAccess(c: AiClient, access: AiAccess) {
    if (c.access === access) return
    try {
      await api.aiClientAccess(c.name, c.program, access)
      clientError = ''
    } catch (e) {
      clientError = String(e)
    }
  }

  async function revoke(c: AiClient) {
    try {
      await api.aiClientRemove(c.name, c.program)
      clientError = ''
    } catch (e) {
      clientError = String(e)
    }
  }

  // ── Журнал ──
  let log = $state<AiLogEntry[]>([])
  let logLoaded = $state(false)
  let logError = $state('')

  async function loadLog() {
    try {
      log = (await api.aiLog(100)).sort((a, b) => b.at_ms - a.at_ms)
      logError = ''
    } catch (e) {
      logError = String(e)
    } finally {
      logLoaded = true
    }
  }

  // Раскрытые строки журнала: какие записи получил ИИ. Подписи берём из
  // локальной истории по id; удалённую запись так и называем.
  const LOG_ITEMS = 20
  let expanded = $state(new Set<string>())
  let seen = $state<Record<string, string>>({})

  function logKey(e: AiLogEntry, i: number) {
    return `${e.at_ms}-${e.tool}-${i}`
  }

  function itemLabel(it: Entry): string {
    if (it.kind === 'image') return t('Картинка {w}×{h}', { w: it.width, h: it.height })
    if (it.kind === 'files') return it.files.map((f) => f.name).join(', ')
    const line = it.text.replace(/\s+/g, ' ').trim()
    return line.length > 120 ? line.slice(0, 120) + '…' : line
  }

  async function toggleLog(key: string, ids: string[]) {
    const next = new Set(expanded)
    if (next.has(key)) {
      next.delete(key)
      expanded = next
      return
    }
    next.add(key)
    expanded = next
    for (const id of ids.slice(0, LOG_ITEMS)) {
      if (seen[id] !== undefined) continue
      try {
        seen[id] = itemLabel(await api.get(id))
      } catch {
        seen[id] = 'gone'
      }
    }
  }

  async function clearLog() {
    try {
      await api.aiLogClear()
      log = []
      expanded = new Set()
    } catch (e) {
      logError = String(e)
    }
  }

  /** Время обращения: сегодня — только часы, раньше — с датой. */
  function when(ms: number): string {
    const d = new Date(ms)
    const loc = lang() === 'en' ? 'en-GB' : 'ru-RU'
    const time = d.toLocaleTimeString(loc, { hour: '2-digit', minute: '2-digit' })
    if (d.toDateString() === new Date().toDateString()) return time
    return `${d.toLocaleDateString(loc, { day: 'numeric', month: 'short' })} ${time}`
  }

  onMount(() => {
    api
      .aiBridgeCommand()
      .then((b) => (bridge = b))
      .catch((e) => (bridgeError = String(e)))
    // Журнал пополняется, пока вкладка открыта: событие — повод перечитать.
    const un = listen('clipwire://ai-log', () => loadLog())
    loadLog()
    return () => un.then((f) => f())
  })
</script>

<div class="card">
  <Toggle
    label={t('Доступ для ИИ-инструментов')}
    hint={t('ИИ-ассистенты (Claude Code, Codex, Cursor и другие) смогут искать по истории и читать записи. Записи уходят в модель ИИ, обычно в облако.')}
    checked={ai.enabled}
    onchange={(v) => saveAi({ enabled: v })}
  />
</div>

<h2>{t('Подключение')}</h2>
<div class="card pad">
  <p class="hint">{t('Команду выполните в терминале, а настройку JSON добавьте в конфиг клиента. Инструменты работают, пока Clipwire запущен.')}</p>
  {#if bridgeError}
    <p class="error" role="alert">{t('Не удалось узнать путь к Clipwire: {error}', { error: bridgeError })}</p>
  {/if}
  {#each snippets as sn (sn.id)}
    <div class="snippet">
      <div class="snippet-head">
        <span>{sn.title}</span>
        <button class="small" onclick={() => copy(sn.id, sn.text)}>{copied === sn.id ? t('Скопировано') : t('Скопировать')}</button>
      </div>
      <code class="code selectable">{sn.text}</code>
    </div>
  {/each}
</div>

<h2>{t('Что видит ИИ')}</h2>
<div class="card pad">
  <div class="kinds">
    <Toggle inline label={t('Текст')} checked={ai.kinds.text} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, text: v } })} />
    <Toggle inline label={t('Форматированный текст')} checked={ai.kinds.rich} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, rich: v } })} />
    <Toggle inline label={t('Ссылки')} checked={ai.kinds.links} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, links: v } })} />
    <Toggle inline label={t('Картинки')} checked={ai.kinds.images} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, images: v } })} />
    <Toggle inline label={t('Файлы')} checked={ai.kinds.files} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, files: v } })} />
    <Toggle inline label={t('Папки')} checked={ai.kinds.folders} onchange={(v) => saveAi({ kinds: { ...s.ai.kinds, folders: v } })} />
  </div>
</div>
<p class="hint outside">{t('Скопированный файл-картинка считается файлом. Вместе с файлами ИИ видит их пути на этом компьютере, и агент с доступом к диску сможет прочитать сами файлы. Записи из программ, которые не синхронизируются, и файлы под правилом «Не отправлять» ИИ не видит.')}</p>

<h2>{t('ИИ видит историю за')}</h2>
<div class="card pad">
  <div class="segmented" role="radiogroup" aria-label={t('ИИ видит историю за')}>
    {#each windows as w (w.days)}
      <button role="radio" aria-checked={ai.history_days === w.days} class:on={ai.history_days === w.days} onclick={() => saveAi({ history_days: w.days })}>{w.title}</button>
    {/each}
  </div>
  <p class="hint">{t('Закреплённое видно всегда, каким бы старым оно ни было.')}</p>
</div>

<div class="card">
  <Toggle
    label={t('Скрывать записи, похожие на пароли и ключи')}
    hint={t('Токены, ключи API, приватные ключи и строки, похожие на пароль, не попадут ни в поиск, ни в ответ на прямой запрос.')}
    checked={ai.hide_secrets}
    beforechange={(v) => {
      if (v) return true
      confirmSecrets = true
      return false
    }}
    onchange={(v) => saveAi({ hide_secrets: v })}
  />
  {#if confirmSecrets}
    <div class="confirm" role="alertdialog">
      <span>{t('ИИ-инструмент сможет прочитать пароли и ключи из истории. Отключить?')}</span>
      <button
        class="danger"
        onclick={() => {
          confirmSecrets = false
          saveAi({ hide_secrets: false })
        }}>{t('Отключить')}</button
      >
      <button bind:this={keepButton} onclick={() => (confirmSecrets = false)}>{t('Отмена')}</button>
    </div>
  {/if}
</div>

<div class="card">
  <Toggle
    label={t('Разрешить ИИ добавлять записи')}
    hint={t('ИИ сможет сохранять текст в историю и класть его в буфер обмена. Такие записи помечены «ИИ» и, как обычные, уходят на все устройства группы.')}
    checked={ai.allow_write}
    onchange={(v) => saveAi({ allow_write: v })}
  />
  <div class="nested">
    <Toggle
      label={t('Спрашивать перед записью в буфер')}
      hint={t('Clipwire покажет текст и спросит, класть ли его в буфер. Без вопроса придёт уведомление.')}
      checked={ai.confirm_clipboard}
      disabled={!ai.allow_write}
      onchange={(v) => saveAi({ confirm_clipboard: v })}
    />
  </div>
</div>

<h2>{t('Подключённые клиенты')}</h2>
{#if clients.length}
  <div class="card list">
    {#each clients as c (`${c.name}\n${c.program ?? ''}`)}
      <div class="client">
        <div class="client-info">
          <span class="client-name">{c.name}</span>
          <span class="mono muted client-program" title={c.program ?? ''}>{c.program ?? t('программа не определена')}</span>
          <span class="mono muted">{t('обращался {when}', { when: ago(c.last_ms) })}</span>
        </div>
        <div class="segmented compact" role="radiogroup" aria-label={t('Доступ для «{client}»', { client: c.name })}>
          {#each accesses as a (a.id)}
            <button role="radio" aria-checked={c.access === a.id} class:on={c.access === a.id} onclick={() => setAccess(c, a.id)}>{a.title}</button>
          {/each}
        </div>
        <button class="small" onclick={() => revoke(c)}>{t('Отозвать')}</button>
      </div>
    {/each}
  </div>
  {#if clientError}<p class="error outside" role="alert">{clientError}</p>{/if}
  <p class="hint outside">{t('Отозванный клиент при следующем обращении снова спросит доступ. Клиента узнают по имени, которое он сам сообщает, и по программе, которая его запустила: то же имя из другой программы снова спросит доступ.')}</p>
{:else}
  <div class="card pad">
    <p class="hint">{t('Пока ни один клиент не подключался. Когда ИИ-инструмент обратится впервые, Clipwire спросит, пускать ли его.')}</p>
  </div>
{/if}

<div class="section-head">
  <h2>{t('Журнал обращений')}</h2>
  {#if log.length}
    <button class="link small" onclick={clearLog}>{t('Очистить журнал')}</button>
  {/if}
</div>
{#if logError}<p class="error" role="alert">{logError}</p>{/if}
{#if log.length}
  <div class="card list log">
    {#each log as e, i (logKey(e, i))}
      {@const key = logKey(e, i)}
      {@const open = expanded.has(key)}
      <div class="log-row">
        <span class="mono muted">{when(e.at_ms)}</span>
        <span class="log-main">
          <span class="log-what" title={e.program ?? t('программа не определена')}
            ><span class="client-name">{e.client}</span> <span class="mono">{e.tool}</span></span
          >
          {#if e.query}<span class="log-query mono muted" title={e.query}>{e.query}</span>{/if}
        </span>
        {#if e.ids.length}
          <button
            class="link small mono log-result"
            aria-expanded={open}
            title={open ? t('Скрыть записи') : t('Показать, какие записи получил ИИ')}
            onclick={() => toggleLog(key, e.ids)}>{e.result}{#if e.count > 1}&nbsp;×{e.count}{/if} {open ? '▴' : '▾'}</button
          >
        {:else}
          <span class="mono muted log-result" title={e.result}>{e.result}{#if e.count > 1}&nbsp;×{e.count}{/if}</span>
        {/if}
      </div>
      {#if open}
        <ul class="log-items">
          {#each e.ids.slice(0, LOG_ITEMS) as id (id)}
            {@const it = seen[id]}
            <li class:muted={!it || it === 'gone'}>
              {#if it === undefined}…{:else if it === 'gone'}{t('Запись удалена')}{:else}{it}{/if}
            </li>
          {/each}
          {#if e.ids.length > LOG_ITEMS}
            <li class="muted">{t('и ещё {n}', { n: e.ids.length - LOG_ITEMS })}</li>
          {/if}
        </ul>
      {/if}
    {/each}
  </div>
{:else if logLoaded}
  <div class="card pad">
    <p class="hint">{t('Обращений пока не было.')}</p>
  </div>
{/if}
<p class="hint outside">{t('Последние 100 обращений. Журнал хранится зашифрованным на этом устройстве, очистка истории его не трогает.')}</p>

<style>
  h2 {
    font-size: var(--text-s);
    font-weight: 400;
    color: var(--c-muted);
    margin: var(--space-5) 0 var(--space-2) 2px;
  }
  .section-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    margin: var(--space-5) 2px var(--space-2);
  }
  .section-head h2 {
    margin: 0;
  }
  .card {
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-l);
    padding: 2px var(--space-4);
    margin-bottom: var(--space-3);
  }
  .card > :global(* + *) {
    border-top: 1px solid var(--c-border);
  }
  .card.pad {
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .card.pad > :global(* + *) {
    border-top: 0;
  }
  .confirm {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
    padding: var(--space-3) 0;
  }
  .confirm span {
    flex: 1 1 100%;
  }
  /* Вложенный переключатель: зависит от верхнего, сдвинут вправо. */
  .nested {
    padding-left: var(--space-4);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: 0;
  }
  .hint.outside {
    margin: 0 2px var(--space-2);
  }
  .error {
    overflow-wrap: anywhere;
    color: var(--c-danger);
    margin: 0;
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .muted {
    color: var(--c-muted);
  }
  .snippet {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .snippet-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .code {
    display: block;
    padding: var(--space-2) var(--space-3);
    background: var(--c-bg);
    border-radius: var(--radius-m);
    font-family: var(--font-mono);
    font-size: var(--text-s);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }
  button {
    align-self: flex-start;
    border: 1px solid var(--c-border);
    background: transparent;
    border-radius: var(--radius-m);
    padding: 7px 14px;
    cursor: pointer;
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-out);
  }
  button:hover:not(:disabled) {
    background: var(--c-hover);
  }
  button:active:not(:disabled) {
    transform: translateY(1px);
  }
  button.small {
    padding: 4px 10px;
    font-size: var(--text-s);
    align-self: center;
  }
  button.danger {
    border-color: var(--c-danger);
    color: var(--c-danger);
  }
  button.link {
    border: 0;
    padding: 0;
    color: var(--c-accent);
    align-self: center;
  }
  button.link:hover {
    background: none;
    text-decoration: underline;
  }
  .kinds {
    display: flex;
    flex-wrap: wrap;
    column-gap: var(--space-5);
  }
  .segmented {
    display: flex;
    gap: 2px;
    padding: 3px;
    background: var(--c-bg);
    border-radius: var(--radius-m);
  }
  .segmented button {
    flex: 1;
    border: 0;
    padding: 5px 8px;
    color: var(--c-muted);
    font-size: var(--text-s);
    align-self: stretch;
  }
  .segmented button.on {
    background: var(--c-surface);
    color: var(--c-text);
  }
  .segmented.compact button {
    flex: none;
    padding: 3px 8px;
  }
  .client {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 10px 0;
    flex-wrap: wrap;
  }
  .client-info {
    flex: 1 1 160px;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .client-name {
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Путь целиком — во всплывающей подсказке. */
  .client-program {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .log {
    max-height: 320px;
    overflow-y: auto;
  }
  .log-row {
    display: grid;
    grid-template-columns: 16ch minmax(0, 1fr) minmax(0, 24ch);
    gap: var(--space-3);
    align-items: baseline;
    padding: 7px 0;
  }
  .log-row > :first-child {
    white-space: nowrap;
  }
  .log-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .log-what,
  .log-query,
  .log-result {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .log-result {
    text-align: right;
  }
  button.log-result {
    justify-self: end;
    max-width: 100%;
  }
  .log-items {
    margin: 0 0 var(--space-2) calc(16ch + var(--space-3));
    padding: var(--space-1) var(--space-3);
    list-style: none;
    border-left: 2px solid var(--c-border);
    font-size: 0.92em;
  }
  .log-items li {
    padding: 2px 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
