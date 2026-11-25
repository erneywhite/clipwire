<script lang="ts">
  // Окно согласия: ИИ-клиент впервые просит доступ к истории или хочет
  // положить текст в буфер. Вопросы стоят в очереди у Rust, окно показывает
  // один, первый. Событие clipwire://consent — повод перечитать очередь; само
  // окно показывает и прячет Rust.
  import { onMount } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { api, type ConsentChoice, type ConsentDto, type Settings } from '../lib/api'
  import { applyTheme } from '../lib/theme'
  import { t, tn } from '../lib/i18n'
  import TitleBar from '../settings/TitleBar.svelte'

  let req = $state<ConsentDto | null>(null)
  let busy = $state(false)
  /**
   * Кнопки у нового вопроса срабатывают не сразу: второй клик двойного клика
   * (или клик под лагом) иначе попадёт в такую же кнопку следующего вопроса,
   * которого человек не видел. Так же браузеры защищают запросы прав. Кнопки
   * при этом не срабатывают, а Esc и закрытие окна работают сразу: они ничего
   * не запоминают.
   */
  const ARM_MS = 700
  let armed = $state(false)
  let error = $state('')
  /**
   * Куда встаёт фокус у нового вопроса. Окно появляется поверх и забирает
   * фокус, пока человек печатает в другой программе, поэтому у вопроса про
   * клиента фокус на заголовке: Enter и пробел там ничего не делают, а отказ
   * запомнился бы. У вопроса про буфер — на «Отклонить», он не запоминается.
   */
  let safeFocus = $state<HTMLElement | null>(null)

  async function refresh() {
    try {
      req = await api.consentPending()
      error = ''
    } catch (e) {
      error = String(e)
    }
  }

  /** force — ответ без кнопки (Esc, закрытие окна): это `dismiss`, ждать не надо. */
  async function answer(choice: ConsentChoice, force = false) {
    if (!req || busy) return
    // Клик до включения кнопок не считается, даже по «Отказать»: отказ в доступе
    // запоминается, и чужой вопрос не должен закрываться вторым кликом.
    if (!armed && !force) return
    busy = true
    try {
      await api.consentAnswer(req.id, choice)
    } catch (e) {
      error = String(e)
    }
    try {
      // busy держится и на время перечитывания: иначе кнопки уже отвеченного
      // вопроса снова нажимаются, и ответ на тот же id уходит второй раз.
      await refresh()
    } finally {
      busy = false
    }
  }

  /**
   * Нет ответа (Esc, «закрыть»): как истёкший срок. Клиенту отказано только на
   * это соединение, ничего не запоминается; текст в буфер не кладётся. Esc,
   * предназначенный другой программе, не закроет клиенту доступ навсегда.
   */
  function dismiss() {
    if (req) answer('dismiss', true)
  }

  // Новый вопрос — фокус на безопасное место: случайный Enter не даст доступ
  // и не запомнит отказ.
  $effect(() => {
    // busy в зависимостях: пока идёт ответ, кнопка выключена и фокус не встанет.
    if (req?.id && safeFocus && !busy) safeFocus.focus()
  })

  // Новый вопрос — кнопки выключены на ARM_MS. Через $derived: эффект
  // перезапускается при смене id, а не при каждом перечитывании очереди.
  const reqId = $derived(req?.id)
  $effect(() => {
    if (!reqId) return
    armed = false
    const timer = setTimeout(() => (armed = true), ARM_MS)
    return () => clearTimeout(timer)
  })

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault()
      dismiss()
    }
  }

  // Длинный текст Rust присылает началом и концом, середину пропускает:
  // хвост тоже на виду, команду в конце за тысячами пробелов не спрятать.
  // Невидимые символы (направление текста, нулевой ширины, `\r`) уже
  // заменены метками ⟨U+202E⟩.
  const shown = $derived(req?.text ?? '')
  const tail = $derived(req?.tail ?? null)

  onMount(() => {
    api.settings().then((s) => applyTheme(s.theme, s.mode)).catch(() => {})
    const un = [
      listen<Settings>('clipwire://settings-changed', (e) => applyTheme(e.payload.theme, e.payload.mode)),
      listen('clipwire://consent', () => refresh()),
    ]
    // Закрытие окна средствами системы (Alt+F4, красная кнопка и Cmd+W на маке,
    // где своей кнопки «закрыть» нет) — тоже «без ответа». Окно не
    // уничтожается: показывает и прячет его Rust.
    un.push(
      getCurrentWindow().onCloseRequested((e) => {
        e.preventDefault()
        if (req) dismiss()
        else getCurrentWindow().hide().catch(() => {})
      }),
    )
    // Сначала подписка, потом запрос: иначе вопрос может проскочить между ними.
    Promise.all(un).then(refresh, refresh)
    return () => un.forEach((p) => p.then((f) => f()))
  })
</script>

<svelte:window {onkeydown} />

<div class="window">
  <TitleBar title={t('Clipwire — запрос ИИ')} onclose={dismiss} />
  {#if req}
    <main>
      {#if req.kind === 'client'}
        <h1 tabindex="-1" bind:this={safeFocus}>{t('«{client}» просит доступ к истории буфера', { client: req.client })}</h1>
        {#if req.client_version}<p class="mono muted">{t('версия {version}', { version: req.client_version })}</p>{/if}
        <p>{t('Он сможет искать по истории и читать записи, которые разрешены в настройках. Прочитанное уходит в модель ИИ, обычно в облако.')}</p>
        {#if req.allow_write}
          <p class="muted">{t('С доступом на запись он сможет добавлять записи в историю и класть текст в буфер обмена.')}</p>
        {/if}
        <p class="muted small">{t('Ответ запомнится. Поменять его можно в настройках, в разделе «ИИ-инструменты».')}</p>
        {#if error}<p class="error" role="alert">{error}</p>{/if}
        <div class="buttons">
          <button class="primary" disabled={busy || !armed} onclick={() => answer('read')}>{t('Только чтение')}</button>
          {#if req.allow_write}
            <button disabled={busy || !armed} onclick={() => answer('read_write')}>{t('Чтение и запись')}</button>
          {/if}
          <button class="danger" disabled={busy} onclick={() => answer('deny')}>{t('Отказать')}</button>
        </div>
      {:else}
        <h1>{t('«{client}» хочет положить в буфер обмена', { client: req.client })}</h1>
        <pre class="text selectable" aria-label={t('Текст для буфера')}>{shown}{#if tail !== null}<span class="gap"
              >{'\n'}{t('… не показано {count} …', {
                count: tn(req.skipped, ['символ', 'символа', 'символов'], ['character', 'characters']),
              })}{'\n'}</span
            >{tail}{/if}</pre>
        {#if req.multiline}
          <p class="warn small" role="note">
            {t('В тексте есть переводы строк: вставленный в терминал, он может сразу выполниться как команда.')}
          </p>
        {/if}
        <p class="mono muted small">
          {tn(req.chars, ['символ', 'символа', 'символов'], ['character', 'characters'])}{#if tail !== null}&nbsp;· {t('показаны начало и конец')}{/if}
        </p>
        {#if error}<p class="error" role="alert">{error}</p>{/if}
        <div class="buttons">
          <button class="primary" disabled={busy || !armed} onclick={() => answer('accept')}>{t('Положить в буфер')}</button>
          <button bind:this={safeFocus} disabled={busy} onclick={() => answer('decline')}>{t('Отклонить')}</button>
        </div>
      {/if}
      <p class="keys muted">{req.kind === 'client' ? t('Esc — ответить позже') : t('Esc — отклонить')}</p>
    </main>
  {:else if error}
    <main><p class="error" role="alert">{error}</p></main>
  {/if}
</div>

<style>
  .window {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--c-bg);
  }
  main {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) var(--space-5) var(--space-4);
  }
  h1 {
    font-size: var(--text-l);
    font-weight: 500;
    margin: 0;
    overflow-wrap: anywhere;
  }
  /* Фокус на заголовке нужен только чтобы увести его с кнопок. */
  h1:focus {
    outline: none;
  }
  p {
    margin: 0;
  }
  .muted {
    color: var(--c-muted);
  }
  .small {
    font-size: var(--text-s);
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .error {
    color: var(--c-danger);
    overflow-wrap: anywhere;
  }
  /* Текст для буфера: моноширинный, с прокруткой, переносы как в тексте. */
  .text {
    flex: 1;
    min-height: 80px;
    margin: 0;
    padding: var(--space-3);
    overflow: auto;
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    font-family: var(--font-mono);
    font-size: var(--text-s);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  /* Пропуск середины длинного текста: не часть текста, выделен цветом. */
  .gap {
    color: var(--c-muted);
    font-style: italic;
    user-select: none;
  }
  .warn {
    color: var(--c-danger);
  }
  .buttons {
    margin-top: auto;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    justify-content: flex-end;
  }
  .keys {
    font-size: var(--text-xs);
    text-align: right;
  }
  button {
    border: 1px solid var(--c-border);
    background: transparent;
    border-radius: var(--radius-m);
    padding: 7px 14px;
    cursor: pointer;
    white-space: nowrap;
    color: var(--c-text);
    transition: background var(--dur-fast) var(--ease-out);
  }
  button:hover:not(:disabled) {
    background: var(--c-hover);
  }
  button:active:not(:disabled) {
    transform: translateY(1px);
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  /* Кольцо и у фокуса, поставленного кодом: видно, что сработает по Enter. */
  button:focus {
    outline: 2px solid var(--c-accent);
    outline-offset: 2px;
  }
  button.primary {
    background: var(--c-accent);
    border-color: var(--c-accent);
    color: var(--c-on-accent);
  }
  button.primary:hover:not(:disabled) {
    background: var(--c-accent);
    filter: brightness(1.08);
  }
  button.danger {
    border-color: var(--c-danger);
    color: var(--c-danger);
  }
</style>
