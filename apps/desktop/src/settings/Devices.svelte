<script lang="ts">
  // Устройства группы: список, удаление устройства и смена ключа группы.
  import { onMount, untrack } from 'svelte'
  import { t } from '../lib/i18n'
  import { listen } from '@tauri-apps/api/event'
  import { api, type DevicesIncoming, type GroupDevice, type RekeyResult, type Status, type StorageKind } from '../lib/api'
  import { ago, plural } from '../lib/format'
  import { CHECKING, adminAction, adminConfirmText, canClaimAdmin, canManage, lastAdmin, checking as checkingText, confirmText, deviceTitle, firstSeenLabel, freshWarning, isAdmin, isFresh, isNewDeviceError, recipients, rekeyChoice, rolesText, sameName, seenLabel, staying, type Roles } from '../lib/devices'

  // `incoming` — итог смены ключа, пришедший событием (его забирает окно
  // настроек: раздел мог быть закрыт). Принятый сообщается `onIncomingTaken`.
  // `serverAdmin` — у устройства есть ключ администратора сервера: оно может
  // забрать роль админа группы, если прежний админ потерян.
  let {
    onRekeyed,
    incoming = null,
    onIncomingTaken,
    serverAdmin = false,
  }: {
    onRekeyed?: (invite: string) => void
    incoming?: DevicesIncoming | null
    onIncomingTaken?: () => void
    serverAdmin?: boolean
  } = $props()

  const fmt = { ago, plural }

  let list = $state<GroupDevice[]>([])
  let loaded = $state(false)
  let error = $state('')
  let busy = $state(false)
  /** Что подтверждаем: id удаляемого устройства, '' — смена ключа без удаления. */
  let confirming = $state<string | null>(null)
  /** Список устройств на момент открытия подтверждения. Человек подтверждает
   *  именно его: `list` за это время может обновиться, а новый ключ должны
   *  получить только те, кого он видел. */
  let shown = $state<GroupDevice[]>([])
  /** Получатели, с которых сняли галочку: они исключаются вместе с удаляемым. */
  let unticked = $state<string[]>([])
  /** Когда открыли подтверждение: от этого момента считается «недавно появилось». */
  let shownAt = $state(0)
  let result = $state<RekeyResult | null>(null)
  let copied = $state(false)
  /** Ответ на смену ключа потерялся, ждём, что скажет сервер при повторном входе. */
  let checking = $state(false)
  /** Вид хранилища файлов группы: у внешнего доступы остаются у тех, кто окажется вне группы. */
  let storageKind = $state<StorageKind | null>(null)
  /** Кто админы группы; null — не знаем (нет связи). */
  let roles = $state<Roles>(null)
  /** Подтверждение роли: кого назначаем админом (`revoke: false`) или с кого
   *  снимаем роль. На старом сервере назначение — это передача роли. */
  let handing = $state<{ id: string; revoke: boolean } | null>(null)
  let handBusy = $state(false)

  const manage = $derived(canManage(roles, list))
  const rolesNote = $derived(rolesText(roles, list))
  const meAdmin = $derived(list.some((d) => d.me && isAdmin(roles, d)))
  const claimable = $derived(canClaimAdmin(roles, list, serverAdmin))
  /** Подтверждение «Стать админом группы». */
  let claiming = $state(false)
  let claimBusy = $state(false)

  /** Забрать роль админа группы себе (администратор сервера). */
  async function claimAdmin() {
    claimBusy = true
    error = ''
    try {
      await api.groupAdminClaim()
      claiming = false
      await load()
    } catch (e) {
      error = String(e)
    } finally {
      claimBusy = false
    }
  }

  const explain = $derived(confirming === null ? [] : confirmText(shown, confirming || null, fmt, storageKind, unticked))
  const keyTo = $derived(confirming === null ? [] : recipients(shown, confirming || null))
  const stay = $derived(confirming === null ? [] : staying(shown, confirming || null, unticked))
  const freshNote = $derived(confirming === null ? '' : freshWarning(shown, confirming || null, unticked, shownAt))

  function toggle(id: string, on: boolean) {
    unticked = on ? unticked.filter((x) => x !== id) : [...unticked, id]
  }

  async function loadStorage() {
    try {
      storageKind = (await api.storageGet()).form.kind
    } catch {
      storageKind = null
    }
  }

  /** Спросить подтверждение удаления (`id`) или смены ключа (`''`). Список
   *  перечитывается и запоминается: человек подтверждает то, что видит.
   *  Вид хранилища к этому моменту тоже мог смениться. */
  async function ask(id: string) {
    result = null
    await load()
    if (error) return
    shown = list
    unticked = []
    shownAt = Date.now()
    confirming = id
    loadStorage()
  }

  async function load() {
    try {
      list = await api.devicesList()
      error = ''
    } catch (e) {
      error = String(e)
    } finally {
      loaded = true
    }
    try {
      roles = await api.groupAdmin()
    } catch {
      roles = null
    }
  }

  /** Назначить или снять админа (сервер с несколькими админами) либо
   *  передать роль (старый сервер). */
  async function handOver() {
    if (!handing) return
    handBusy = true
    error = ''
    try {
      const { id, revoke } = handing
      if (!roles?.multi) await api.deviceMakeAdmin(id)
      else roles = revoke ? await api.deviceRevokeAdmin(id) : await api.deviceGrantAdmin(id)
      handing = null
      await load()
    } catch (e) {
      error = String(e)
    } finally {
      handBusy = false
    }
  }

  async function run() {
    if (confirming === null) return
    busy = true
    error = ''
    checking = false
    try {
      const id = confirming
      // В команду уходит показанный список, а не свежий: устройство, которого
      // в диалоге не было, движок заметит и смену отменит.
      const pick = rekeyChoice(shown, id || null, unticked)
      done(id ? await api.deviceRemove(id, pick.confirmed, pick.remove) : await api.groupRekey(pick.confirmed, pick.remove))
    } catch (e) {
      const msg = String(e)
      // Текст приходит из Rust на языке интерфейса; русский — на случай старых сообщений.
      if (msg.includes(CHECKING) || msg.includes(checkingText())) {
        checking = true
        confirming = null
      } else {
        error = msg
        // Состав группы изменился: закрыть вопрос и показать новый список.
        if (isNewDeviceError(msg)) {
          confirming = null
          load().then(() => (error = msg))
        }
      }
    } finally {
      busy = false
    }
  }

  function done(r: RekeyResult) {
    result = r
    confirming = null
    copied = false
    checking = false
    onRekeyed?.(r.invite)
  }

  async function copyInvite() {
    if (!result) return
    await api.copyQuietly(result.invite)
    copied = true
  }

  // Вход устройства и его карточка приходят почти разом: одно чтение на пачку.
  let reloadTimer: ReturnType<typeof setTimeout> | undefined
  function loadSoon() {
    clearTimeout(reloadTimer)
    reloadTimer = setTimeout(load, 300)
  }

  // Ключ сменили здесь, а ответ сервера потерялся: успех приходит событием,
  // когда устройство снова вошло в старую группу. Или новый ключ не дошёл.
  $effect(() => {
    const inc = incoming
    if (!inc) return
    untrack(() => {
      if (inc.result) done(inc.result)
      if (inc.error) {
        checking = false
        error = inc.error
      }
      onIncomingTaken?.()
    })
  })

  onMount(() => {
    // Список обновляется, когда сервис снова на связи (в том числе после
    // смены ключа): раньше движок ответить не может.
    const unlisten = [
      listen<Status>('clipwire://status', (e) => {
        if (e.payload.kind === 'online') load()
      }),
      // Сервер сообщил, что устройство подключилось, ушло или переименовалось.
      listen('clipwire://devices-changed', loadSoon),
    ]
    load()
    return () => {
      clearTimeout(reloadTimer)
      unlisten.forEach((p) => p.then((f) => f()))
    }
  })
</script>

<h2>{t('Устройства группы')}</h2>
<div class="card">
  {#if !loaded}
    <p class="row hint">{t('Загружаю список…')}</p>
  {:else if list.length === 0}
    <p class="row hint">{error ? t('Список недоступен, пока нет связи с сервером.') : t('В группе пока нет устройств.')}</p>
  {/if}
  {#each list as d (d.id)}
    <div class="row device">
      <span class="dot" class:on={d.online}></span>
      <div class="who">
        <span class="name" class:unknown={!d.known}>{deviceTitle(d)}{#if d.me}<span class="me">{' · '}{t('это устройство')}</span>{/if}{#if isAdmin(roles, d)}<span class="badge">{t('админ')}</span>{/if}</span>
        <span class="meta mono">{d.os ?? t('нет карточки')} · {d.fingerprint} · {seenLabel(d, fmt)}</span>
      </div>
      {#if roles?.multi}
        {#if adminAction(roles, list, d) === 'grant'}
          <button disabled={busy || handBusy} onclick={() => (handing = { id: d.id, revoke: false })}>{t('Сделать админом')}</button>
        {:else if adminAction(roles, list, d) === 'revoke'}
          <button
            disabled={busy || handBusy || lastAdmin(roles, list, d)}
            title={lastAdmin(roles, list, d) ? t('Это последний админ группы. Сначала сделайте админом другое устройство.') : undefined}
            onclick={() => (handing = { id: d.id, revoke: true })}>{t('Снять админа')}</button>
        {/if}
      {:else if !d.me && meAdmin && d.known}
        <button disabled={busy || handBusy} onclick={() => (handing = { id: d.id, revoke: false })}>{t('Сделать админом')}</button>
      {/if}
      {#if !d.me && manage}
        <button class="danger" disabled={busy} onclick={() => ask(d.id)}>{t('Удалить')}</button>
      {/if}
    </div>
    {#if handing?.id === d.id && roles?.multi}
      {@const q = adminConfirmText(d, handing.revoke)}
      <div class="row confirm" role="alertdialog">
        <strong>{q.title}</strong>
        <p class="hint">{q.hint}</p>
        <div class="buttons">
          <button class="primary" disabled={handBusy} onclick={handOver}>{handBusy ? t('Сохраняю…') : handing.revoke ? t('Снять админа') : t('Сделать админом')}</button>
          <button disabled={handBusy} onclick={() => (handing = null)}>{t('Отмена')}</button>
        </div>
      </div>
    {:else if handing?.id === d.id}
      <div class="row confirm" role="alertdialog">
        <strong>{t('Сделать «{name}» админом группы?', { name: deviceTitle(d) })}</strong>
        <p class="hint">{t('Это устройство перестанет быть админом: удалять устройства и менять ключ группы сможет только «{name}». Вернуть роль сможет только оно.', { name: deviceTitle(d) })}</p>
        <div class="buttons">
          <button class="primary" disabled={handBusy} onclick={handOver}>{handBusy ? t('Передаю…') : t('Сделать админом')}</button>
          <button disabled={handBusy} onclick={() => (handing = null)}>{t('Отмена')}</button>
        </div>
      </div>
    {/if}
  {/each}
</div>
{#if rolesNote}
  <p class="hint outside">{rolesNote}</p>
{/if}
{#if claimable}
  <div class="card pad">
    {#if claiming}
      <div class="confirm" role="alertdialog">
        <strong>{t('Стать админом группы?')}</strong>
        {#if roles?.multi}
          <p class="hint">{t('Это устройство станет ещё одним админом группы. Это нужно, если прежние админы потеряны: потом их можно снять. Сервер разрешит это только с действующим ключом администратора сервера.')}</p>
        {:else}
          <p class="hint">{t('Нынешний админ перестанет им быть: удалять устройства и менять ключ группы сможет только это устройство. Это нужно, если прежний админ потерян. Сервер разрешит это только с действующим ключом администратора сервера.')}</p>
        {/if}
        <div class="buttons">
          <button class="primary" disabled={claimBusy} onclick={claimAdmin}>{claimBusy ? t('Передаю…') : t('Стать админом')}</button>
          <button disabled={claimBusy} onclick={() => (claiming = false)}>{t('Отмена')}</button>
        </div>
      </div>
    {:else}
      <button disabled={busy || handBusy} onclick={() => (claiming = true)}>{t('Стать админом группы (вы администратор сервера)')}</button>
    {/if}
  </div>
{/if}
{#if list.some((d) => !d.known && !d.me)}
  <p class="hint outside">
    {t('У неизвестных устройств старая версия Clipwire или карточка не открылась. При смене ключа новый ключ им передать нечем: обновите их и откройте на них Clipwire, тогда имя появится.')}
  </p>
{/if}

<div class="card pad">
  {#if confirming !== null}
    <div class="confirm">
      <strong>{confirming ? t('Удалить устройство и сменить ключ группы?') : t('Сменить ключ группы?')}</strong>
      {#each explain as line (line)}
        <p class="hint" class:warn={line.startsWith('Внимание') || line.startsWith('Warning')}>{line}</p>
      {/each}
      <div class="keyto">
        <span class="hint">{keyTo.length > 0 ? t('Новый ключ получат:') : t('Кроме этого устройства, новый ключ не получит никто.')}</span>
        {#each keyTo as d (d.id)}
          <label class="pick" class:fresh={isFresh(d, shownAt) || sameName(stay, d)}>
            <input type="checkbox" checked={!unticked.includes(d.id)} disabled={busy} onchange={(e) => toggle(d.id, e.currentTarget.checked)} />
            <span class="who">
              <span class="name">{deviceTitle(d)}{#if isFresh(d, shownAt)}<span class="badge new">{t('недавно')}</span>{/if}{#if sameName(stay, d)}<span class="badge new">{t('имя повторяется')}</span>{/if}</span>
              <span class="meta mono">{d.os ?? '?'} · {d.fingerprint} · {firstSeenLabel(d, fmt, shownAt)}</span>
            </span>
          </label>
        {/each}
        {#if freshNote}<p class="hint warn">{freshNote}</p>{/if}
      </div>
      <div class="buttons">
        <button class="danger" disabled={busy} onclick={run}>
          {busy ? t('Меняю ключ…') : confirming ? t('Удалить устройство') : t('Сменить ключ')}
        </button>
        <button disabled={busy} onclick={() => (confirming = null)}>{t('Отмена')}</button>
      </div>
    </div>
  {:else}
    <p class="hint">
      {t('Новый ключ получат все устройства группы, а старый код приглашения перестанет работать. Пригодится, если код приглашения попал не туда.')}
    </p>
    <button disabled={busy || !loaded || list.length === 0 || !manage} onclick={() => ask('')}>
      {t('Сменить ключ группы')}
    </button>
  {/if}
  {#if checking}<p class="hint" role="status">{checkingText()}</p>{/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if result}
    <div class="done" role="status">
      <p>{t('Ключ группы сменён. Новый код приглашения:')}</p>
      <code class="code selectable">{result.invite}</code>
      <div class="buttons">
        <button onclick={copyInvite}>{copied ? t('Скопировано') : t('Скопировать код')}</button>
        <button onclick={() => (result = null)}>{t('Скрыть')}</button>
      </div>
      {#if result.skipped.length > 0}
        <p class="hint warn">
          {t('Не получили новый ключ: {names}. Подключите их заново по этому коду.', { names: result.skipped.join(', ') })}
        </p>
      {/if}
    </div>
  {/if}
</div>

<style>
  h2 {
    font-size: var(--text-s);
    font-weight: 400;
    color: var(--c-muted);
    margin: var(--space-5) 0 var(--space-2) 2px;
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
  .row {
    padding: 11px 0;
    margin: 0;
  }
  .device {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--c-border);
  }
  .dot.on {
    background: var(--st-online, var(--c-accent));
  }
  .who {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .name {
    overflow-wrap: anywhere;
  }
  .name.unknown {
    color: var(--c-muted);
    font-style: italic;
  }
  .me {
    color: var(--c-muted);
    font-style: normal;
  }
  .badge {
    display: inline-block;
    margin-left: var(--space-2);
    padding: 0 6px;
    border: 1px solid var(--c-accent);
    border-radius: var(--radius-s, 4px);
    color: var(--c-accent);
    font-family: var(--font-mono);
    font-size: var(--text-xs, 11px);
    font-style: normal;
    line-height: 1.6;
    vertical-align: 1px;
  }
  .meta {
    color: var(--c-muted);
    overflow-wrap: anywhere;
  }
  .keyto {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .pick {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 6px var(--space-3);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    cursor: pointer;
  }
  .pick.fresh {
    border-color: var(--c-danger);
  }
  .pick input {
    flex: none;
    margin: 0;
    accent-color: var(--c-accent);
  }
  .badge.new {
    border-color: var(--c-danger);
    color: var(--c-danger);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: 0;
  }
  .hint.outside {
    margin: 0 2px var(--space-2);
  }
  .hint.warn {
    color: var(--c-danger);
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
  .confirm,
  .done {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .done p {
    margin: 0;
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .error {
    color: var(--c-danger);
    margin: 0;
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
  .device button {
    align-self: center;
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
  button.danger {
    border-color: var(--c-danger);
    color: var(--c-danger);
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
</style>
