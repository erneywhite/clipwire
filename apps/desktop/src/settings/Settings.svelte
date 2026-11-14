<script lang="ts">
  import { isMac, keys } from '../lib/platform'
  import { onMount } from 'svelte'
  import { t } from '../lib/i18n'
  import { listen } from '@tauri-apps/api/event'
  import { enable, disable, isEnabled } from '@tauri-apps/plugin-autostart'
  import { check, type Update } from '@tauri-apps/plugin-updater'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { api, type DeviceInfo, type GroupDevice, type GroupRekeyed, type Retention, type Settings, type StatsOverview, type Status, type StorageState, type Theme } from '../lib/api'
  import { rekeyedBy } from '../lib/devices'
  import { applyTheme } from '../lib/theme'
  import * as fmt from '../lib/format'
  import { bytesLabel } from '../lib/format'
  import { devicesLabel, historyLabel, lastSyncLabel, linkLabel, quotaLabel, serverFilesLabel, serverItemsLabel, storageLabel } from '../lib/overview'
  import { fromEvent, label as hotkeyLabel } from '../lib/hotkey'
  import Toggle from './Toggle.svelte'
  import TitleBar from './TitleBar.svelte'
  import Devices from './Devices.svelte'
  import Storage from './Storage.svelte'

  type Section = 'general' | 'sync' | 'privacy' | 'keys' | 'about'
  const sections: { id: Section; title: string; icon: string }[] = [
    { id: 'general', title: t('Основные'), icon: 'M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0' },
    { id: 'sync', title: t('Синхронизация'), icon: 'M4 12a8 8 0 0 1 14-5.3M20 12a8 8 0 0 1-14 5.3M18 3v4h-4M6 21v-4h4' },
    { id: 'privacy', title: t('Приватность'), icon: 'M6 11V8a6 6 0 0 1 12 0v3M5 11h14v10H5z' },
    { id: 'keys', title: t('Клавиши'), icon: 'M3 7h18v10H3zM7 11h0M11 11h0M15 11h0M8 14h8' },
    { id: 'about', title: t('О программе'), icon: 'M12 8h0M11 12h1v5h1M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z' },
  ]
  const themes: { id: Theme; title: string }[] = [
    { id: 'graphite', title: t('Графит') },
    { id: 'paper', title: t('Бумага') },
    { id: 'signal', title: t('Сигнал') },
  ]
  const retentions: { id: Retention; title: string }[] = [
    { id: 'day', title: t('День') },
    { id: 'week', title: t('Неделя') },
    { id: 'month', title: t('Месяц') },
    { id: 'year', title: t('Год') },
    { id: 'forever', title: t('Всегда') },
  ]

  let section = $state<Section>('general')
  let s = $state<Settings | null>(null)
  let status = $state<Status>({ kind: 'connecting' })
  let info = $state<DeviceInfo | null>(null)
  let autostart = $state(false)
  let error = $state('')
  let notice = $state('')

  // Первый запуск
  let serverCode = $state('')
  let inviteInput = $state('')
  let createdInvite = $state('')
  let busy = $state(false)

  // Синхронизация
  let invite = $state('')
  let confirmLeave = $state(false)
  let confirmClear = $state(false)
  // Кэш скачанных файлов: размер, null — ещё не знаем.
  let cacheBytes = $state<number | null>(null)
  let cacheBusy = $state(false)

  // Клавиши
  let recording = $state(false)
  let newApp = $state('')

  // Обновления
  let update = $state<Update | null>(null)
  let updateState = $state<'idle' | 'checking' | 'none' | 'available' | 'installing' | 'error'>('idle')
  let updateError = $state('')
  /** Версия, скачанная в фоне: поставится сама, когда программа не нужна. */
  let updateReady = $state<string | null>(null)

  const configured = $derived(status.kind !== 'unconfigured' && status.kind !== 'local')
  let retrying = $state(false)

  async function retry() {
    retrying = true
    error = ''
    try {
      await api.groupRetry()
      notice = ''
    } catch (e) {
      error = String(e)
    } finally {
      retrying = false
    }
  }

  function flash(msg: string) {
    notice = msg
    setTimeout(() => (notice === msg ? (notice = '') : null), 2500)
  }

  async function save(patch: Partial<Settings>) {
    if (!s) return
    const next = { ...s, ...patch }
    try {
      await api.saveSettings(next)
      s = next
      error = ''
      applyTheme(next.theme, next.mode)
    } catch (e) {
      error = String(e)
      s = await api.settings()
    }
  }

  /** Порог загрузки: целое число мегабайт, не меньше 0. Мусор в поле — вернуть прежнее. */
  function saveUploadLimit(el: HTMLInputElement) {
    if (!s) return
    const v = Math.floor(Number(el.value))
    if (el.value.trim() === '' || !Number.isFinite(v) || v < 0) {
      el.value = String(s.file_upload_limit_mb)
      return
    }
    el.value = String(v)
    if (v !== s.file_upload_limit_mb) save({ file_upload_limit_mb: v })
  }

  async function refresh() {
    status = await api.status()
    info = await api.deviceInfo()
  }

  async function createGroup() {
    busy = true
    error = ''
    try {
      createdInvite = await api.createGroup(serverCode.trim())
      await refresh()
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  async function joinGroup() {
    busy = true
    error = ''
    try {
      await api.joinGroup(inviteInput.trim())
      await refresh()
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  async function copy(text: string) {
    await api.copyQuietly(text)
    flash(t('Скопировано'))
  }

  async function leave() {
    await api.leaveGroup()
    confirmLeave = false
    invite = ''
    createdInvite = ''
    await refresh()
  }

  async function loadCacheSize() {
    try {
      cacheBytes = await api.cacheSize()
    } catch {
      cacheBytes = null
    }
  }

  async function clearCache() {
    cacheBusy = true
    try {
      const r = await api.cacheClear()
      cacheBytes = r.size
      flash(
        r.failed > 0
          ? t('Удалено не всё: {failed} — файлы открыты в другой программе', { failed: r.failed })
          : t('Кэш скачанного очищен'),
      )
    } catch (e) {
      error = String(e)
    } finally {
      cacheBusy = false
    }
  }

  // Размер кэша — при каждом открытии раздела: он меняется от скачиваний и чистки.
  $effect(() => {
    if (section === 'sync' || section === 'about') loadCacheSize()
  })

  // «О программе» → «Состояние». Каждое значение грузится само: старый сервер
  // или нет связи портят только свою строку.
  let stats = $state<StatsOverview | null>(null)
  let groupDevices = $state<GroupDevice[] | null>(null)
  let devicesError = $state('')
  let storageState = $state<StorageState | null>(null)
  let overviewBusy = $state(false)

  async function loadOverview() {
    overviewBusy = true
    const tasks: Promise<unknown>[] = [api.statsOverview().then((v) => (stats = v)).catch(() => (stats = null))]
    if (configured) {
      tasks.push(
        api
          .devicesList()
          .then((v) => ((groupDevices = v), (devicesError = '')))
          .catch((e) => ((groupDevices = null), (devicesError = String(e)))),
        api.storageGet().then((v) => (storageState = v)).catch(() => (storageState = null)),
      )
    }
    await Promise.all(tasks)
    overviewBusy = false
  }

  // При открытии раздела и когда меняется связь с сервером.
  $effect(() => {
    if (section === 'about' && status.kind) loadOverview()
  })

  async function clearHistory() {
    await api.clear(true)
    confirmClear = false
    flash(t('История очищена, закреплённое осталось'))
  }

  async function toggleAutostart(on: boolean) {
    try {
      if (on) await enable()
      else await disable()
      autostart = await isEnabled()
    } catch (e) {
      error = String(e)
    }
  }

  function recordKey(e: KeyboardEvent) {
    if (!recording) return
    e.preventDefault()
    e.stopPropagation()
    if (e.key === 'Escape') {
      recording = false
      return
    }
    const accel = fromEvent(e)
    if (accel) {
      recording = false
      save({ popup_hotkey: accel })
    }
  }

  // Размер окна вставки одним числом: ширина и высота растут вместе.
  const SIZE = { w0: 320, w1: 560, h0: 400, h1: 760 }
  function valueToSize(v: number) {
    const k = v / 100
    return {
      w: Math.round((SIZE.w0 + (SIZE.w1 - SIZE.w0) * k) / 10) * 10,
      h: Math.round((SIZE.h0 + (SIZE.h1 - SIZE.h0) * k) / 10) * 10,
    }
  }
  function sizeToValue(w: number) {
    return Math.round(((w - SIZE.w0) / (SIZE.w1 - SIZE.w0)) * 100)
  }

  let demoSize = $state({ w: 0, h: 0 })
  // Экран в логических пикселях — для уменьшенной схемы «экран и окно на нём».
  const screenW = window.screen.width || 1920
  const screenH = window.screen.height || 1080
  // Пример окна вставки рядом с настройками (отдельное окно, которое не
  // забирает фокус). Размер меняется не чаще раза в кадр, в работе один запрос.
  let previewOpen = false
  let pending: { w: number; h: number } | null = null
  let previewBusy = false
  async function flush() {
    if (previewBusy || !pending) return
    previewBusy = true
    const next = pending
    pending = null
    try {
      await api.sizePreview(next.w, next.h)
    } finally {
      previewBusy = false
      if (pending) requestAnimationFrame(flush)
    }
  }
  function previewShow() {
    clearTimeout(hideTimer)
    if (previewOpen) return
    previewOpen = true
    pending = demoSize
    requestAnimationFrame(flush)
  }
  function previewResize(v: number) {
    demoSize = valueToSize(v)
    if (!previewOpen) return
    pending = demoSize
    requestAnimationFrame(flush)
  }
  // Прячем не мгновенно: через 250 мс проверяем, что курсор правда ушёл с блока.
  let sizeCard: HTMLElement | undefined = $state()
  let hideTimer: ReturnType<typeof setTimeout> | undefined
  // Курсор ушёл — прячем, даже если ползунок в фокусе после перетаскивания.
  // Фокус ушёл с ползунка — прячем, только если курсора над блоком нет.
  function previewHide() {
    clearTimeout(hideTimer)
    hideTimer = setTimeout(() => {
      if (!previewOpen || sizeCard?.matches(':hover')) return
      previewOpen = false
      pending = null
      api.sizePreviewHide()
    }, 250)
  }
  $effect(() => {
    if (section !== 'keys' && previewOpen) {
      previewOpen = false
      pending = null
      api.sizePreviewHide()
    }
  })

  function addApp() {
    const name = newApp.trim()
    if (!s || !name || s.ignored_apps.some((a) => a.toLowerCase() === name.toLowerCase())) return
    save({ ignored_apps: [...s.ignored_apps, name] })
    newApp = ''
  }

  async function checkUpdates() {
    updateState = 'checking'
    updateError = ''
    try {
      update = await check()
      updateState = update ? 'available' : 'none'
      updateReady = await api.updateReady()
    } catch (e) {
      updateError = String(e)
      updateState = 'error'
    }
  }

  async function installUpdate() {
    if (!update) return
    updateState = 'installing'
    try {
      // Ставит Rust: движок останавливается так же, как при выходе, и
      // программа перезапускается уже новой версией.
      await api.updateInstall()
    } catch (e) {
      updateError = String(e)
      updateState = 'error'
    }
  }

  function statusText(st: Status): string {
    switch (st.kind) {
      case 'online':
        return t('На связи с сервером')
      case 'connecting':
        return t('Подключение…')
      case 'paused':
        return t('На паузе')
      case 'offline':
        return t('Нет связи: {detail}', { detail: st.detail })
      case 'local':
        return t('Только это устройство')
      case 'unconfigured':
        return t('Синхронизация не запустилась')
      case 'excluded':
        return t('Устройство исключено из группы')
    }
  }

  function statusTone(st: Status): 'ok' | 'muted' | 'error' {
    if (st.kind === 'online') return 'ok'
    if (st.kind === 'connecting' || st.kind === 'paused' || st.kind === 'local') return 'muted'
    return 'error'
  }

  onMount(() => {
    api.settings().then((v) => {
      s = v
      demoSize = { w: v.popup_width, h: v.popup_height }
      applyTheme(v.theme, v.mode)
    })
    isEnabled().then((v) => (autostart = v)).catch(() => {})
    // Страница могла перезагрузиться при открытом примере окна — спрятать его.
    api.sizePreviewHide()
    // Сначала подписка на события, потом запрос статуса: иначе событие о старте
    // сервиса может прийти раньше подписки и потеряться.
    const unlisten = [
      listen<Status>('clipwire://status', (e) => (status = e.payload)),
      // Фоновая проверка нашла обновление — сразу показать кнопку в «О программе».
      listen('clipwire://update-available', () => checkUpdates()),
      listen<string>('clipwire://update-ready', (e) => (updateReady = e.payload)),
      listen<Settings>('clipwire://settings-changed', (e) => {
        s = e.payload
        applyTheme(e.payload.theme, e.payload.mode)
      }),
      // Смена ключа группы: старый код приглашения больше не действует.
      listen<GroupRekeyed>('clipwire://group-rekeyed', (e) => {
        invite = ''
        createdInvite = ''
        if (!e.payload.by_me) notice = t('{who}. Старый код приглашения больше не действует.', { who: rekeyedBy(e.payload.by) })
        refresh()
      }),
      listen('clipwire://group-excluded', () => {
        invite = ''
        createdInvite = ''
        section = 'sync'
        refresh()
      }),
    ]
    Promise.all(unlisten).then(refresh)
    return () => unlisten.forEach((p) => p.then((f) => f()))
  })
</script>

<svelte:window onkeydown={recordKey} />

{#if s}
  <div class="window">
  <TitleBar />
  <div class="layout">
    <nav aria-label={t('Разделы настроек')}>
      {#each sections as sec (sec.id)}
        <button class:active={section === sec.id} onclick={() => (section = sec.id)} aria-current={section === sec.id}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d={sec.icon} /></svg>
          {sec.title}
        </button>
      {/each}
      <div class="status" data-kind={status.kind}>
        <span class="dot" aria-hidden="true"></span>
        <span>{statusText(status)}</span>
      </div>
    </nav>

    <main>
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      {#if notice}<p class="notice" role="status">{notice}</p>{/if}

      {#if section === 'general'}
        <h1>{t('Основные')}</h1>
        <div class="card">
          <Toggle label={t('Запускать при входе в систему')} checked={autostart} onchange={toggleAutostart} />
          <Toggle
            label={t('Звук при копировании')}
            hint={t('Короткий щелчок, когда в историю попадает новое: скопированное здесь или пришедшее с другого устройства.')}
            checked={s.sound_on_receive}
            onchange={(v) => save({ sound_on_receive: v })}
          />
          <Toggle
            label={t('Обновляться автоматически')}
            hint={t('Новая версия скачивается в фоне и ставится, когда вы не пользуетесь Clipwire. Выключено — обновление ставится по кнопке в «О программе» или в меню трея.')}
            checked={s.auto_update}
            onchange={(v) => save({ auto_update: v })}
          />
        </div>

        <h2>{t('Язык')}</h2>
        <div class="card pad">
          <!-- Названия языков — на самих языках, их не переводим. -->
          <div class="segmented" role="radiogroup" aria-label={t('Язык')}>
            {#each [['system', t('Как в системе')], ['ru', 'Русский'], ['en', 'English']] as [id, title] (id)}
              <button role="radio" aria-checked={s.language === id} class:on={s.language === id} onclick={() => save({ language: id as Settings['language'] })}>{title}</button>
            {/each}
          </div>
        </div>

        <h2>{t('Оформление')}</h2>
        <div class="card pad">
          <div class="themes">
            {#each themes as th (th.id)}
              <button class="theme" class:on={s.theme === th.id} data-preview={th.id} onclick={() => save({ theme: th.id })} aria-pressed={s.theme === th.id}>
                <span class="swatch" aria-hidden="true"><span></span></span>
                {th.title}
              </button>
            {/each}
          </div>
          <div class="segmented" role="radiogroup" aria-label={t('Режим')}>
            {#each [['system', t('Как в системе')], ['light', t('Светлая')], ['dark', t('Тёмная')]] as [id, title] (id)}
              <button role="radio" aria-checked={s.mode === id} class:on={s.mode === id} onclick={() => save({ mode: id as Settings['mode'] })}>{title}</button>
            {/each}
          </div>
        </div>

        <h2>{t('Вставка')}</h2>
        <div class="card">
          <Toggle
            label={t('Сразу вставлять в активное окно')}
            hint={t('Двойной клик или Enter вставляет туда, где стоит курсор. Иначе элемент только попадает в буфер.')}
            checked={s.paste_into_active}
            onchange={(v) => save({ paste_into_active: v })}
          />
        </div>

        <h2>{t('Хранить историю')}</h2>
        <div class="card pad">
          <input
            type="range"
            min="0"
            max="4"
            step="1"
            value={retentions.findIndex((r) => r.id === s?.retention)}
            oninput={(e) => save({ retention: retentions[Number(e.currentTarget.value)].id })}
            aria-label={t('Срок хранения истории')}
          />
          <div class="ticks">
            {#each retentions as r (r.id)}<span class:on={s.retention === r.id}>{r.title}</span>{/each}
          </div>
          <p class="hint">{t('Закреплённое хранится всегда. История лежит только на этом устройстве и зашифрована.')}</p>
          {#if confirmClear}
            <div class="confirm">
              <span>{t('Удалить всю историю, кроме закреплённого?')}</span>
              <button class="danger" onclick={clearHistory}>{t('Удалить')}</button>
              <button onclick={() => (confirmClear = false)}>{t('Отмена')}</button>
            </div>
          {:else}
            <button onclick={() => (confirmClear = true)}>{t('Очистить историю')}</button>
          {/if}
        </div>
      {:else if section === 'sync'}
        <h1>{t('Синхронизация')}</h1>
        {#if status.kind === 'excluded'}
          <div class="card pad">
            <h2 class="in-card">{t('Устройство исключено из группы')}</h2>
            <p class="hint">
              {t('Сервер сообщил, что этого устройства больше нет в группе: его удалили на другом устройстве, ключ группы сменили, пока оно было не в сети дольше 30 дней, или новый ключ пришёл от устройства, которого здесь не видели. Синхронизация остановлена. Ключ группы и история остались на этом устройстве.')}
            </p>
            <p class="hint">
              {t('Если вы этого не делали, попробуйте снова. Если устройство и правда удалили, выйдите из группы и подключитесь заново по новому коду приглашения.')}
            </p>
            <div class="buttons">
              <button class="danger" disabled={retrying} onclick={leave}>{t('Выйти из группы')}</button>
              <button disabled={retrying} onclick={retry}>{retrying ? t('Подключаю…') : t('Попробовать снова')}</button>
            </div>
          </div>
        {:else if !configured}
          <p class="lead">
            {t('Сейчас Clipwire работает только на этом устройстве: история и окно вставки есть, синхронизации нет. Чтобы буфер был общим с другими компьютерами, подключите сервер. Группу создаёт первое устройство, остальные подключаются по коду приглашения.')}
          </p>
          <div class="card pad">
            <h2 class="in-card">{t('Это первое устройство')}</h2>
            <p class="hint">{t('Код сервера печатает сервер Clipwire при запуске, строка начинается с')} <code>cwserver1-</code>.</p>
            <textarea bind:value={serverCode} rows="3" placeholder="cwserver1-…" spellcheck="false"></textarea>
            <button class="primary" disabled={busy || !serverCode.trim()} onclick={createGroup}>{t('Создать группу')}</button>
          </div>
          <div class="card pad">
            <h2 class="in-card">{t('Подключить к группе')}</h2>
            <p class="hint">{t('Код приглашения берётся на любом устройстве группы: Синхронизация → «Показать код».')}</p>
            <textarea bind:value={inviteInput} rows="3" placeholder="clipwire1-…" spellcheck="false"></textarea>
            <button class="primary" disabled={busy || !inviteInput.trim()} onclick={joinGroup}>{t('Подключить')}</button>
          </div>
        {:else}
          {#if createdInvite}
            <div class="card pad accent">
              <h2 class="in-card">{t('Группа создана')}</h2>
              <p class="hint">{t('Этот код вставьте на остальных устройствах. В нём ключ шифрования группы: передавайте его только себе.')}</p>
              <code class="code selectable">{createdInvite}</code>
              <button onclick={() => copy(createdInvite)}>{t('Скопировать код')}</button>
            </div>
          {/if}
          <div class="card">
            <div class="kv"><span>{t('Состояние')}</span><span>{statusText(status)}</span></div>
            <div class="kv"><span>{t('Сервер')}</span><span class="mono">{info?.relay_url ?? '—'}</span></div>
            <div class="kv"><span>{t('Отпечаток устройства')}</span><span class="mono">{info?.device_id ?? '—'}</span></div>
          </div>

          <h2>{t('Это устройство')}</h2>
          <div class="card pad">
            <label class="field">
              <span>{t('Имя, которое видят остальные устройства')}</span>
              <input value={s.device_name} onchange={(e) => save({ device_name: e.currentTarget.value.trim() || s!.device_name })} />
            </label>
          </div>
          <div class="card">
            <Toggle
              label={t('Удалять на всех устройствах')}
              hint={t('Запись, удалённая здесь, пропадает и на остальных устройствах группы, в том числе на выключенных: они удалят её, когда подключатся (если не пройдёт больше срока хранения на сервере). Закреплённое на другом устройстве остаётся. Выключено — удаление только здесь.')}
              checked={s.delete_everywhere}
              onchange={(v) => save({ delete_everywhere: v })}
            />
          </div>

          <h2>{t('Файлы')}</h2>
          <div class="card pad">
            <label class="field">
              <span>{t('Загружать в хранилище файлы до N МБ')}</span>
              <input
                class="num"
                type="number"
                min="0"
                step="1"
                inputmode="numeric"
                value={s.file_upload_limit_mb}
                onchange={(e) => saveUploadLimit(e.currentTarget)}
              />
            </label>
            <p class="hint">
              {t('Скопированные файлы такого размера сразу уходят зашифрованными в хранилище группы (сервер Clipwire или своё, см. «Хранилище файлов»), и другие устройства вставляют их, даже когда этот компьютер выключен. 0 — не загружать. Файлы крупнее порога другие устройства берут напрямую с этого компьютера, пока он в сети.')}
            </p>
            <div class="cache">
              <span>{t('Скачанные файлы занимают')} <span class="mono">{cacheBytes === null ? '—' : bytesLabel(cacheBytes)}</span></span>
              <button disabled={cacheBusy || !cacheBytes} onclick={clearCache}>{t('Очистить кэш скачанного')}</button>
            </div>
            <p class="hint">
              {t('Здесь лежат файлы, скачанные с других устройств и с сервера. Копия удаляется сама, когда удаляется её запись в истории. Очистка не трогает то, что скачивается прямо сейчас; удалённое скачается заново при вставке.')}
            </p>
          </div>

          <Storage />

          <h2>{t('Добавить устройство')}</h2>
          <div class="card pad">
            <p class="hint">{t('Код приглашения содержит ключ шифрования группы. Кто его увидел, тот может читать ваш буфер.')}</p>
            {#if invite}
              <code class="code selectable">{invite}</code>
              <div class="buttons">
                <button onclick={() => copy(invite)}>{t('Скопировать код')}</button>
                <button onclick={() => (invite = '')}>{t('Скрыть')}</button>
              </div>
            {:else}
              <button onclick={async () => (invite = await api.inviteCode())}>{t('Показать код')}</button>
            {/if}
          </div>

          <Devices onRekeyed={() => ((invite = ''), (createdInvite = ''))} />

          <h2>{t('Отключить')}</h2>
          <div class="card pad">
            {#if confirmLeave}
              <div class="confirm">
                <span>{t('Удалить ключи группы с этого устройства? Локальная история останется.')}</span>
                <button class="danger" onclick={leave}>{t('Отключить')}</button>
                <button onclick={() => (confirmLeave = false)}>{t('Отмена')}</button>
              </div>
            {:else}
              <button onclick={() => (confirmLeave = true)}>{t('Отключить это устройство от группы')}</button>
            {/if}
          </div>
        {/if}
      {:else if section === 'privacy'}
        <h1>{t('Приватность')}</h1>
        <div class="card">
          <Toggle
            label={t('Скрывать окна при демонстрации экрана')}
            hint={t('Окна Clipwire не попадут в запись экрана и трансляцию.')}
            checked={s.hide_from_capture}
            onchange={(v) => save({ hide_from_capture: v })}
          />
          <Toggle
            label={t('Загружать карточки ссылок')}
            hint={t('По пробелу подгружается заголовок и картинка страницы. Похожие на одноразовые ссылки не загружаются никогда.')}
            checked={s.link_previews}
            onchange={(v) => save({ link_previews: v })}
          />
        </div>
        <p class="hint outside">{t('Содержимое, которое менеджеры паролей помечают как приватное, не синхронизируется никогда, эта защита не отключается.')}</p>

        <h2>{t('Не синхронизировать из программ')}</h2>
        <div class="card pad">
          <ul class="apps">
            {#each s.ignored_apps as app (app)}
              <li>
                <span class="mono">{app}</span>
                <button aria-label={t('Убрать {app}', { app })} onclick={() => save({ ignored_apps: s!.ignored_apps.filter((a) => a !== app) })}>✕</button>
              </li>
            {:else}
              <li class="hint">{t('Список пуст')}</li>
            {/each}
          </ul>
          <form class="add" onsubmit={(e) => { e.preventDefault(); addApp() }}>
            <input bind:value={newApp} placeholder={isMac ? 'KeePassXC' : 'KeePass.exe'} spellcheck="false" aria-label={t('Имя программы')} />
            <button type="submit" disabled={!newApp.trim()}>{t('Добавить')}</button>
          </form>
        </div>
      {:else if section === 'keys'}
        <h1>{t('Клавиши')}</h1>
        <div class="card">
          <div class="kv">
            <span>{t('Окно вставки')}</span>
            <button class="key" class:rec={recording} onclick={() => (recording = !recording)}>
              {recording ? t('Нажмите сочетание… (Esc — отмена)') : hotkeyLabel(s.popup_hotkey)}
            </button>
          </div>
        </div>

        <h2>{t('Размер окна вставки')}</h2>
        <div
          bind:this={sizeCard}
          class="card pad"
          role="group"
          aria-label={t('Размер окна вставки')}
          onmouseenter={() => previewShow()}
          onmouseleave={() => previewHide()}
        >
          <input
            type="range"
            min="0"
            max="100"
            step="1"
            value={sizeToValue(s.popup_width)}
            oninput={(e) => previewResize(Number(e.currentTarget.value))}
            onchange={(e) => {
              const { w, h } = valueToSize(Number(e.currentTarget.value))
              save({ popup_width: w, popup_height: h })
            }}
            onfocus={() => previewShow()}
            onblur={() => previewHide()}
            aria-label={t('Размер окна вставки')}
          />
          <div class="ticks"><span>{t('Компактное')}</span><span>{demoSize.w} × {demoSize.h}</span><span>{t('Просторное')}</span></div>
          <div class="size-preview">
            <div class="screen" style:aspect-ratio="{screenW} / {screenH}" title={t('Ваш экран в масштабе')}>
              <div
                class="screen-popup"
                style:width="{(demoSize.w / screenW) * 100}%"
                style:height="{(demoSize.h / screenH) * 100}%"
              ></div>
              <span class="cursor" aria-hidden="true"></span>
            </div>
            <p class="hint">
              {t('Пока курсор над этим блоком, рядом с настройками показан пример окна вставки в настоящем размере. Схема слева — ваш экран и окно на нём в той же пропорции.')}
            </p>
          </div>
        </div>

        <h2>{t('В окне вставки')}</h2>
        <div class="card">
          {#each [['↑ ↓', t('выбрать')], [t('Enter, двойной клик'), t('вставить')], [keys('Ctrl + 1…9'), t('вставить элемент по номеру')], [t('Пробел'), t('предпросмотр (при пустом поиске)')], [keys('Ctrl + P'), t('закрепить')], ...(isMac ? [['⌘⌫', t('удалить из истории')]] : [['Delete', t('удалить из истории (при пустом поиске)')], ['Shift + Delete', t('удалить, когда в поиске есть текст')]]), [t('Правый клик, клавиша меню'), t('действия с записью')], ['Tab', t('История ↔ Закреплённое')], [keys('Alt + ← →'), t('фильтр: текст, ссылки, картинки, файлы')], ['Esc', t('закрыть')]] as [k, v] (k)}
            <div class="kv"><span class="mono">{k}</span><span>{v}</span></div>
          {/each}
        </div>
      {:else if section === 'about'}
        <h1>{t('О программе')}</h1>
        <div class="card pad">
          <div class="update-head">
            <div>
              <p class="version">Clipwire <span class="mono">{info?.version ?? '—'}</span></p>
              {#if updateState === 'available' && update}
                <p class="accent-text">{t('Доступна версия {version}', { version: update.version })}</p>
              {:else if updateState === 'installing'}
                <p class="hint">{t('Скачивание и установка…')}</p>
              {:else if updateState === 'none'}
                <p class="hint">{t('Установлена последняя версия')}</p>
              {/if}
            </div>
            {#if updateState === 'available' && update}
              <button class="primary" onclick={installUpdate}>
                {s.auto_update && updateReady === update.version ? t('Установить сейчас') : t('Установить и перезапустить')}
              </button>
            {:else if updateState !== 'installing'}
              <button disabled={updateState === 'checking'} onclick={checkUpdates}>
                {updateState === 'checking' ? t('Проверка…') : t('Проверить обновления')}
              </button>
            {/if}
          </div>
          {#if updateState === 'available' && update && s.auto_update && updateReady === update.version}
            <p class="hint">{t('Уже скачана и установится сама, когда вы закроете окна Clipwire и пару минут не будете им пользоваться.')}</p>
          {/if}
          {#if updateState === 'error'}<p class="error">{t('Не удалось проверить обновления: {error}', { error: updateError })}</p>{/if}
          <p class="hint">{t('Обновления скачиваются с GitHub и проверяются по цифровой подписи.')}</p>
        </div>

        <div class="section-head">
          <h2>{t('Состояние')}</h2>
          <button class="link small" disabled={overviewBusy} onclick={loadOverview}>{overviewBusy ? t('Обновляю…') : t('Обновить')}</button>
        </div>
        <div class="card">
          <div class="kv">
            <span>{t('Сервер')}</span>
            <span class="tone" data-tone={statusTone(status)}>{statusText(status)}</span>
          </div>
          {#if configured && stats?.link}
            <div class="kv">
              <span>{t('Связь')}</span>
              <span>{linkLabel(stats.link)}</span>
            </div>
          {/if}
          {#if configured && stats}
            <div class="kv">
              <span>{t('Синхронизация')}</span>
              <span>{lastSyncLabel(stats.last_sent_ms, stats.last_received_ms, fmt)}</span>
            </div>
          {/if}
          {#if configured}
            {@const storage = storageState ? storageLabel(storageState) : null}
            <div class="kv">
              <span>{t('Хранилище файлов')}</span>
              <span class="tone" data-tone={storage?.tone ?? 'muted'}>{storage?.text ?? '—'}</span>
            </div>
          {/if}
          <div class="kv">
            <span>{t('История на этом устройстве')}</span>
            <span>{stats ? historyLabel(stats.history_total, stats.history_pinned, fmt) : '—'}</span>
          </div>
          <div class="kv">
            <span>{t('Скачанные файлы')}</span>
            <span>{cacheBytes === null ? '—' : cacheBytes === 0 ? t('Нет') : bytesLabel(cacheBytes)}</span>
          </div>
          {#if configured}
            <div class="kv">
              <span>{t('История группы на сервере')}</span>
              {#if stats?.server}
                <span>{serverItemsLabel(stats.server, fmt)}</span>
              {:else}
                <span class="tone" data-tone="muted">{stats?.server_error ?? '—'}</span>
              {/if}
            </div>
            <div class="kv">
              <span>{t('Файлы на сервере')}</span>
              <span>{stats?.server ? serverFilesLabel(stats.server, fmt) : '—'}</span>
            </div>
            {#if stats?.server}
              {@const q = quotaLabel(stats.server, fmt)}
              <div class="kv">
                <span>{t('Место под файлы группы')}</span>
                <span class="tone" data-tone={q.tone}>{q.text}</span>
              </div>
            {/if}
            <div class="kv">
              <span>{t('Устройств в группе')}</span>
              {#if groupDevices}
                <span>{devicesLabel(groupDevices)}</span>
              {:else}
                <span class="tone" data-tone="muted">{devicesError || '—'}</span>
              {/if}
            </div>
          {/if}
        </div>
        {#if configured}
          <p class="hint outside">{t('На сервере всё лежит зашифрованным и удаляется по сроку, заданному на сервере.')}</p>
        {/if}

        <h2>{t('О проекте')}</h2>
        <div class="card">
          <div class="kv"><span>{t('Исходный код')}</span><button class="link" onclick={() => openUrl('https://github.com/erneywhite/clipwire')}>github.com/erneywhite/clipwire</button></div>
          <div class="kv"><span>{t('Лицензия')}</span><span>AGPL-3.0</span></div>
        </div>
        <div class="card pad donate">
          <div>
            <p class="donate-title">{t('Нравится Clipwire?')}</p>
            <p class="hint">{t('Программа бесплатная. Если она экономит вам время, можно угостить автора кофе: это правда помогает её развивать.')}</p>
          </div>
          <button class="kofi" onclick={() => openUrl('https://ko-fi.com/erneywhite')}>
            <svg viewBox="0 0 24 24" aria-hidden="true">
              <path class="cup" d="M4 7h12v6a5 5 0 0 1-5 5H9a5 5 0 0 1-5-5zM16 9h1.5a2.5 2.5 0 0 1 0 5H16" />
              <path class="heart" d="M10 15.2 7.6 12.9a1.5 1.5 0 0 1 2.4-1.8 1.5 1.5 0 0 1 2.4 1.8z" />
            </svg>
            Buy me a coffee
          </button>
        </div>
      {/if}
    </main>
  </div>
  </div>
{/if}

<style>
  .donate {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  .donate p {
    margin: 0;
  }
  .donate-title {
    font-weight: 600;
    margin-bottom: var(--space-1) !important;
  }
  /* Кнопка Ko-fi в одном виде во всех темах, как на других проектах автора. */
  .kofi {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding: 9px 16px;
    border: 0;
    border-radius: var(--radius-m);
    background: #b58cf7;
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .kofi:hover {
    background: #a374ef;
  }
  .kofi:focus-visible {
    outline: 2px solid var(--c-accent);
    outline-offset: 2px;
  }
  .kofi svg {
    width: 22px;
    height: 22px;
    flex: none;
    border-radius: 50%;
    background: #fff;
    padding: 2px;
  }
  .kofi .cup {
    fill: none;
    stroke: #29abe0;
    stroke-width: 1.8;
    stroke-linejoin: round;
  }
  .kofi .heart {
    fill: #ff5e5b;
  }
  .window {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .layout {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 200px minmax(0, 1fr);
  }
  nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--space-4) var(--space-2);
    background: var(--c-surface);
    border-right: 1px solid var(--c-border);
  }
  nav button {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    border: 0;
    background: none;
    padding: 8px var(--space-3);
    border-radius: var(--radius-m);
    text-align: left;
    color: var(--c-muted);
    cursor: pointer;
    white-space: nowrap;
  }
  nav button:hover {
    background: var(--c-hover);
    color: var(--c-text);
  }
  nav button.active {
    background: var(--c-sel);
    color: var(--c-text);
  }
  nav svg {
    width: 17px;
    height: 17px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
    flex: none;
  }
  nav button.active svg {
    stroke: var(--c-accent);
  }
  .status {
    margin-top: auto;
    display: flex;
    gap: var(--space-2);
    align-items: center;
    padding: var(--space-2) var(--space-3);
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .status span:last-child {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
    background: var(--st-paused);
  }
  .status[data-kind='online'] .dot {
    background: var(--st-online);
  }
  .status[data-kind='local'] .dot {
    background: var(--st-local);
  }
  .status[data-kind='paused'] .dot {
    background: var(--st-paused);
  }
  .status[data-kind='offline'] .dot,
  .status[data-kind='excluded'] .dot,
  .status[data-kind='unconfigured'] .dot {
    background: var(--st-error);
  }
  /* Подключение — жёлтый с лёгкой пульсацией, как провод в трее. */
  .status[data-kind='connecting'] .dot {
    background: var(--st-connecting);
    animation: pulse 1.4s var(--ease-out) infinite alternate;
  }
  @keyframes pulse {
    to {
      opacity: 0.4;
    }
  }
  main {
    overflow-y: auto;
    padding: var(--space-5) var(--space-5) var(--space-5);
  }
  h1 {
    font-size: var(--text-xl);
    font-weight: 500;
    margin: 0 0 var(--space-4);
  }
  h2 {
    font-size: var(--text-s);
    font-weight: 400;
    color: var(--c-muted);
    margin: var(--space-5) 0 var(--space-2) 2px;
  }
  h2.in-card {
    font-size: var(--text-l);
    font-weight: 500;
    color: var(--c-text);
    margin: 0;
  }
  .lead {
    color: var(--c-muted);
    margin: 0 0 var(--space-4);
    max-width: 60ch;
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
  .card.accent {
    border-color: var(--c-accent);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: 0;
  }
  .hint.outside {
    margin: 0 2px var(--space-2);
  }
  .kv {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: var(--space-4);
    padding: 11px 0;
  }
  .kv > span:first-child {
    color: var(--c-muted);
  }
  .kv > :last-child {
    text-align: right;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .mono,
  code {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .code {
    display: block;
    padding: var(--space-3);
    background: var(--c-bg);
    border-radius: var(--radius-m);
    overflow-wrap: anywhere;
    user-select: all;
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
  button:disabled {
    opacity: 0.5;
    cursor: default;
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
  .cache {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    flex-wrap: wrap;
    margin-top: var(--space-3);
  }
  .buttons,
  .confirm {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .confirm span {
    flex: 1 1 100%;
  }
  textarea,
  input:not([type='range']):not([type='checkbox']) {
    width: 100%;
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 8px var(--space-3);
    color: var(--c-text);
    resize: vertical;
  }
  textarea {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    flex: 1;
    min-width: 0;
  }
  .field > span {
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .field input.num {
    width: 12ch;
    font-family: var(--font-mono);
  }
  .themes {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: var(--space-2);
  }
  .theme {
    align-self: stretch;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    align-items: center;
    padding: var(--space-2);
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .theme.on {
    border-color: var(--c-accent);
    color: var(--c-text);
  }
  /* Одна рамка: кольцо фокуса только с клавиатуры, и оно заменяет рамку, а не добавляется. */
  .theme:focus-visible {
    outline: none;
    border-color: var(--c-accent);
    box-shadow: 0 0 0 1px var(--c-accent);
  }
  .swatch {
    width: 100%;
    height: 36px;
    border-radius: var(--radius-s);
    display: flex;
    align-items: flex-end;
    padding: 6px;
    border: 1px solid var(--c-border);
  }
  .swatch span {
    height: 4px;
    width: 60%;
    border-radius: 2px;
  }
  /* Превью тем рисуются собственными цветами тем, независимо от текущей. */
  .theme[data-preview='graphite'] .swatch { background: #1c1e22; }
  .theme[data-preview='graphite'] .swatch span { background: #e2a63b; }
  .theme[data-preview='paper'] .swatch { background: #f3efe6; }
  .theme[data-preview='paper'] .swatch span { background: #2d55c8; }
  .theme[data-preview='signal'] .swatch { background: #0f1517; }
  .theme[data-preview='signal'] .swatch span { background: #46cfa3; }
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
  input[type='range'] {
    width: 100%;
    accent-color: var(--c-accent);
  }
  .ticks {
    display: flex;
    justify-content: space-between;
    font-size: var(--text-xs);
    color: var(--c-muted);
    margin-top: calc(-1 * var(--space-2));
  }
  .ticks .on {
    color: var(--c-text);
  }
  .size-preview {
    display: flex;
    gap: var(--space-4);
    align-items: flex-start;
  }
  .screen {
    position: relative;
    flex: 0 0 170px;
    border: 1px solid var(--c-border);
    border-radius: var(--radius-s);
    background: var(--c-bg);
  }
  .screen-popup {
    position: absolute;
    left: 40%;
    top: 30%;
    border-radius: 2px;
    background: color-mix(in oklch, var(--c-accent) 30%, transparent);
    border: 1px solid var(--c-accent);
  }
  .cursor {
    position: absolute;
    left: calc(40% - 4px);
    top: calc(30% - 4px);
    width: 0;
    height: 0;
    border-left: 4px solid var(--c-text);
    border-bottom: 4px solid transparent;
  }

  .apps {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .apps li {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 6px 0;
  }
  .apps li + li {
    border-top: 1px solid var(--c-border);
  }
  .apps button {
    border: 0;
    padding: 2px 8px;
    color: var(--c-muted);
  }
  .add {
    display: flex;
    gap: var(--space-2);
  }
  .key {
    font-family: var(--font-mono);
    font-size: var(--text-s);
    align-self: center;
  }
  .key.rec {
    border-color: var(--c-accent);
    color: var(--c-accent);
  }
  .error {
    color: var(--c-danger);
    margin: 0 0 var(--space-3);
  }
  .notice {
    color: var(--c-accent);
    margin: 0 0 var(--space-3);
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
  button.link.small {
    font-size: var(--text-s);
  }
  .update-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  .update-head p {
    margin: 0;
  }
  .update-head button {
    align-self: center;
  }
  .version {
    font-weight: 600;
    margin-bottom: var(--space-1) !important;
  }
  .accent-text {
    color: var(--c-accent);
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
  .tone[data-tone='ok'] {
    color: var(--c-text);
  }
  .tone[data-tone='error'] {
    color: var(--c-danger);
  }
  .tone[data-tone='warn'] {
    color: var(--st-connecting);
  }
  .tone[data-tone='muted'] {
    color: var(--c-muted);
  }
</style>
