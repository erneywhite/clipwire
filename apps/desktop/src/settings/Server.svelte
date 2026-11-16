<script lang="ts">
  // Раздел «Сервер» (только у администратора сервера): сводка, группы
  // сервера с удалением, одноразовые коды создания группы, пределы сервера, смена токена доступа и ключа
  // администратора.
  import { onMount } from 'svelte'
  import { t } from '../lib/i18n'
  import { api, type GroupInvites, type RelayRotated, type ServerGroups, type ServerSetting, type ServerSummary } from '../lib/api'
  import {
    BYTE_UNITS,
    DEFAULT_INVITE_TTL,
    INVITE_TTLS,
    inviteIssuedText,
    inviteLabel,
    inviteState,
    inviteStateText,
    inviteTtlText,
    sortInvites,
    collectChanges,
    deleteGroupText,
    groupActivity,
    groupDevicesText,
    groupMuted,
    groupSizeText,
    groupStateText,
    sortGroups,
    draftsFor,
    groupSettings,
    resetDraft,
    settingText,
    summaryRows,
    switchUnit,
    unitLabel,
    valueText,
    type ByteUnit,
    type Draft,
  } from '../lib/server'

  // `onForgotten` — ключ администратора забыт: раздел пора убрать.
  // `onRelayRotated` — токен сменён, у этого устройства новый код приглашения.
  let { onForgotten, onRelayRotated }: { onForgotten?: () => void; onRelayRotated?: () => void } = $props()

  let summary = $state<ServerSummary | null>(null)
  let summaryError = $state('')
  let settings = $state<ServerSetting[] | null>(null)
  let drafts = $state<Record<string, Draft>>({})
  let loadError = $state('')
  let loading = $state(false)
  let saving = $state(false)
  let saveError = $state('')
  let savedNote = $state(false)

  /** Какое действие подтверждаем. */
  let confirming = $state<'relay' | 'admin' | 'forget' | null>(null)
  let acting = $state(false)
  let actionError = $state('')
  let relayResult = $state<RelayRotated | null>(null)
  let adminCode = $state('')
  let copied = $state('')

  let groupsInfo = $state<ServerGroups | null>(null)
  let groupsError = $state('')
  /** Группа, удаление которой подтверждаем (id). */
  let confirmGroup = $state<string | null>(null)
  let deletingGroup = $state(false)
  let groupError = $state('')
  const groupList = $derived(groupsInfo ? sortGroups(groupsInfo.groups) : [])

  let invitesInfo = $state<GroupInvites | null>(null)
  let invitesError = $state('')
  let inviteLabelInput = $state('')
  let inviteTtl = $state(DEFAULT_INVITE_TTL)
  let issuing = $state(false)
  /** Только что выпущенный код: показывается один раз. */
  let issuedCode = $state('')
  let inviteError = $state('')
  /** Код, отзыв которого подтверждаем (id). */
  let confirmInvite = $state<number | null>(null)
  let revoking = $state(false)
  const inviteList = $derived(invitesInfo ? sortInvites(invitesInfo.invites) : [])

  const pending = $derived(settings ? collectChanges(settings, drafts) : { changes: [], errors: {} as Record<string, string> })
  const hasErrors = $derived(Object.keys(pending.errors).length > 0)
  const dirty = $derived(pending.changes.length > 0 || hasErrors)
  const groups = $derived(settings ? groupSettings(settings) : [])

  async function loadSummary() {
    try {
      summary = await api.serverSummary()
      summaryError = ''
    } catch (e) {
      summary = null
      summaryError = String(e)
    }
  }

  async function loadGroups() {
    try {
      groupsInfo = await api.serverGroups()
      groupsError = ''
    } catch (e) {
      groupsInfo = null
      groupsError = String(e)
    }
  }

  async function loadInvites() {
    try {
      invitesInfo = await api.serverInvites()
      invitesError = ''
    } catch (e) {
      invitesInfo = null
      invitesError = String(e)
    }
  }

  async function issueInvite() {
    issuing = true
    inviteError = ''
    try {
      const r = await api.serverInviteCreate(inviteLabelInput.trim(), inviteTtl)
      issuedCode = r.code
      invitesInfo = { supported: true, invites: r.invites }
      inviteLabelInput = ''
      copied = ''
    } catch (e) {
      inviteError = String(e)
    } finally {
      issuing = false
    }
  }

  async function revokeInvite(id: number) {
    revoking = true
    inviteError = ''
    try {
      const invites = await api.serverInviteRevoke(id)
      invitesInfo = { supported: true, invites }
      confirmInvite = null
    } catch (e) {
      inviteError = String(e)
    } finally {
      revoking = false
    }
  }

  function askDelete(id: string) {
    confirmGroup = id
    groupError = ''
  }

  async function deleteGroup(id: string) {
    deletingGroup = true
    groupError = ''
    try {
      const groups = await api.serverGroupDelete(id)
      groupsInfo = { supported: true, groups }
      confirmGroup = null
      loadSummary()
    } catch (e) {
      groupError = String(e)
    } finally {
      deletingGroup = false
    }
  }

  async function load() {
    loading = true
    try {
      const list = await api.serverSettings()
      settings = list
      drafts = draftsFor(list)
      loadError = ''
    } catch (e) {
      loadError = String(e)
    } finally {
      loading = false
    }
    loadSummary()
    loadGroups()
    loadInvites()
  }

  async function save() {
    if (!settings || hasErrors || pending.changes.length === 0) return
    saving = true
    saveError = ''
    try {
      const list = await api.serverSettingsSet(pending.changes)
      settings = list
      drafts = draftsFor(list)
      savedNote = true
      setTimeout(() => (savedNote = false), 2500)
      loadSummary()
    } catch (e) {
      saveError = String(e)
    } finally {
      saving = false
    }
  }

  function revert() {
    if (settings) drafts = draftsFor(settings)
    saveError = ''
  }

  function edit(key: string, text: string) {
    drafts[key] = { ...drafts[key], text, reset: false }
  }

  function setUnit(s: ServerSetting, unit: ByteUnit) {
    drafts[s.key] = switchUnit(s, drafts[s.key], unit)
  }

  function ask(what: 'relay' | 'admin' | 'forget') {
    confirming = what
    actionError = ''
  }

  async function act() {
    const what = confirming
    if (!what) return
    acting = true
    actionError = ''
    try {
      if (what === 'relay') {
        relayResult = await api.serverRotateRelayToken()
        onRelayRotated?.()
      } else if (what === 'admin') {
        adminCode = await api.serverRotateAdminKey()
      } else {
        await api.adminForget()
        onForgotten?.()
      }
      confirming = null
      copied = ''
    } catch (e) {
      actionError = String(e)
    } finally {
      acting = false
    }
  }

  async function copy(text: string, what: string) {
    await api.copyQuietly(text)
    copied = what
  }

  onMount(load)
</script>

<div class="section-head">
  <h2>{t('Сводка')}</h2>
  <button class="link small" disabled={loading} onclick={load}>{loading ? t('Обновляю…') : t('Обновить')}</button>
</div>
<div class="card list">
  {#if summary}
    {#each summaryRows(summary) as [k, v] (k)}
      <div class="kv"><span>{k}</span><span class="mono">{v}</span></div>
    {/each}
  {:else}
    <p class="kv hint">{summaryError || t('Загружаю…')}</p>
  {/if}
</div>

<h2>{t('Группы')}</h2>
{#if groupsError}
  <div class="card pad"><p class="error" role="alert">{groupsError}</p></div>
{:else if !groupsInfo}
  <div class="card pad"><p class="hint">{t('Загружаю…')}</p></div>
{:else if !groupsInfo.supported}
  <div class="card pad">
    <p class="hint">{t('Сервер не показывает группы: обновите образ сервера Clipwire, и здесь появится список групп с удалением.')}</p>
  </div>
{:else if groupList.length === 0}
  <div class="card pad"><p class="hint">{t('Групп на сервере нет.')}</p></div>
{:else}
  <p class="hint outside">
    {t('Имён у групп нет: сервер знает только их id и ничего не может расшифровать. Группа узнаётся по отпечатку id, группу этого устройства отмечает «ваша группа».')}
  </p>
  <div class="card list">
    {#each groupList as g (g.id)}
      <div class="group">
        <div class="group-row" class:muted={groupMuted(g)}>
          <div class="group-main">
            <div class="group-title">
              <span class="mono">{g.fingerprint}</span>
              {#if g.mine}<span class="tag accent">{t('ваша группа')}</span>{/if}
              {#if !g.has_admin && g.state === 'live'}<span class="tag">{t('без админа')}</span>{/if}
              {#if groupStateText(g)}<span class="tag">{groupStateText(g)}</span>{/if}
            </div>
            <div class="group-meta">{groupDevicesText(g)} · {groupActivity(g.last_seen_ms)}</div>
            <div class="group-meta mono">{groupSizeText(g)}</div>
          </div>
          {#if !g.mine && confirmGroup !== g.id}
            <button class="small danger" disabled={deletingGroup} onclick={() => askDelete(g.id)}>{t('Удалить')}</button>
          {/if}
        </div>
        {#if confirmGroup === g.id}
          <div class="confirm group-confirm" role="alertdialog">
            <strong>{t('Удалить группу {fp}?', { fp: g.fingerprint })}</strong>
            {#each deleteGroupText(g) as p, i (i)}
              <p class="hint" class:warn={i > 0}>{p}</p>
            {/each}
            <div class="buttons">
              <button class="danger" disabled={deletingGroup} onclick={() => deleteGroup(g.id)}>{deletingGroup ? t('Удаляю…') : t('Удалить группу')}</button>
              <button disabled={deletingGroup} onclick={() => (confirmGroup = null)}>{t('Отмена')}</button>
            </div>
            {#if groupError}<p class="error" role="alert">{groupError}</p>{/if}
          </div>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<h2>{t('Коды создания группы')}</h2>
{#if invitesError}
  <div class="card pad"><p class="error" role="alert">{invitesError}</p></div>
{:else if !invitesInfo}
  <div class="card pad"><p class="hint">{t('Загружаю…')}</p></div>
{:else if !invitesInfo.supported}
  <div class="card pad">
    <p class="hint">{t('Сервер не знает кодов создания группы: обновите образ сервера Clipwire.')}</p>
  </div>
{:else}
  <p class="hint outside">
    {t('С таким кодом другой человек создаёт на этом сервере одну свою группу. Он станет админом своей группы, но не администратором сервера. Код одноразовый.')}
  </p>
  <div class="card pad">
    <div class="field">
      <label for="invite-label">{t('Для кого')}</label>
      <input id="invite-label" class="wide" maxlength="80" bind:value={inviteLabelInput} placeholder={t('Например, Маша')} />
    </div>
    <div class="field">
      <span id="invite-ttl-label">{t('Срок действия')}</span>
      <div class="segmented" role="radiogroup" aria-labelledby="invite-ttl-label">
        {#each INVITE_TTLS as h (h)}
          <button role="radio" aria-checked={inviteTtl === h} class:on={inviteTtl === h} onclick={() => (inviteTtl = h)}>{inviteTtlText(h)}</button>
        {/each}
      </div>
    </div>
    <button class="primary" disabled={issuing} onclick={issueInvite}>{issuing ? t('Выпускаю…') : t('Выпустить код')}</button>
    {#if issuedCode}
      <div class="done" role="status">
        <p>{t('Код выпущен. Скопируйте его сейчас: сервер хранит только его хэш, и показать код ещё раз нельзя.')}</p>
        <code class="code selectable">{issuedCode}</code>
        <div class="buttons">
          <button onclick={() => copy(issuedCode, 'invite-code')}>{copied === 'invite-code' ? t('Скопировано') : t('Скопировать код')}</button>
          <button onclick={() => (issuedCode = '')}>{t('Скрыть')}</button>
        </div>
        <p class="hint">{t('Человек вставляет код в «Синхронизация» → «Создать группу». Передайте его так, чтобы никто другой не увидел: кто первым введёт код, тот и создаст группу.')}</p>
      </div>
    {/if}
    {#if inviteError}<p class="error" role="alert">{inviteError}</p>{/if}
  </div>
  {#if inviteList.length > 0}
    <div class="card list">
      {#each inviteList as inv (inv.id)}
        {@const st = inviteState(inv)}
        <div class="group">
          <div class="group-row" class:muted={st !== 'active'}>
            <div class="group-main">
              <div class="group-title">
                <span>{inviteLabel(inv)}</span>
                <span class="tag" class:accent={st === 'active'}>{inviteStateText(inv)}</span>
              </div>
              <div class="group-meta">{inviteIssuedText(inv)}</div>
            </div>
            {#if st === 'active' && confirmInvite !== inv.id}
              <button class="small danger" disabled={revoking} onclick={() => (confirmInvite = inv.id)}>{t('Отозвать')}</button>
            {/if}
          </div>
          {#if confirmInvite === inv.id}
            <div class="confirm group-confirm" role="alertdialog">
              <strong>{t('Отозвать код «{label}»?', { label: inviteLabel(inv) })}</strong>
              <p class="hint">{t('Создать группу по этому коду больше не получится.')}</p>
              <div class="buttons">
                <button class="danger" disabled={revoking} onclick={() => revokeInvite(inv.id)}>{revoking ? t('Отзываю…') : t('Отозвать код')}</button>
                <button disabled={revoking} onclick={() => (confirmInvite = null)}>{t('Отмена')}</button>
              </div>
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
{/if}

<h2>{t('Пределы сервера')}</h2>
<p class="hint outside">
  {t('Новые значения действуют сразу, перезапускать сервер не нужно. Значения по умолчанию берутся из переменных окружения контейнера; «Сбросить» возвращает поле к ним.')}
</p>
{#if loadError}
  <div class="card pad"><p class="error" role="alert">{loadError}</p></div>
{:else if !settings}
  <div class="card pad"><p class="hint">{t('Загружаю…')}</p></div>
{:else}
  {#each groups as g (g.title)}
    <h3>{g.title}</h3>
    <div class="card pad">
      {#each g.items as s (s.key)}
        {@const d = drafts[s.key]}
        {@const text = settingText(s.key)}
        {@const err = pending.errors[s.key]}
        <div class="field">
          <label for="srv-{s.key}">{text.title}</label>
          <div class="input-row">
            <input
              id="srv-{s.key}"
              class="num"
              class:invalid={!!err}
              inputmode="decimal"
              value={d.text}
              oninput={(e) => edit(s.key, e.currentTarget.value)}
              aria-invalid={!!err}
              aria-describedby="srv-{s.key}-hint"
            />
            {#if s.unit === 'bytes'}
              <div class="segmented small" role="radiogroup" aria-label={t('Единица')}>
                {#each BYTE_UNITS as u (u)}
                  <button role="radio" aria-checked={d.unit === u} class:on={d.unit === u} onclick={() => setUnit(s, u)}>{unitLabel(u)}</button>
                {/each}
              </div>
            {:else if s.unit === 'hours'}
              <span class="unit">{t('ч')}</span>
            {/if}
            {#if s.stored && !d.reset}
              <button class="small" onclick={() => (drafts[s.key] = resetDraft(s))}>{t('Сбросить')}</button>
            {/if}
          </div>
          {#if err}<span class="bad" role="alert">{err}</span>{/if}
          <span class="hint" id="srv-{s.key}-hint">
            {#if text.hint}{text.hint}{' '}{/if}
            {t('По умолчанию: {value}.', { value: valueText(s, s.default) })}
            {#if d.reset}<span class="accent-text">{t('Вернётся к нему при сохранении.')}</span>{/if}
          </span>
        </div>
      {/each}
    </div>
  {/each}
  <div class="save-bar" class:dirty>
    <button class="primary" disabled={saving || hasErrors || pending.changes.length === 0} onclick={save}>
      {saving ? t('Сохраняю…') : t('Сохранить пределы')}
    </button>
    {#if dirty}<button disabled={saving} onclick={revert}>{t('Отменить правки')}</button>{/if}
    {#if savedNote}<span class="ok" role="status">{t('Сохранено')}</span>{/if}
    {#if hasErrors}<span class="bad">{t('Исправьте поля с ошибками')}</span>{/if}
  </div>
  {#if saveError}<p class="error outside" role="alert">{saveError}</p>{/if}
{/if}

<h2>{t('Доступ к серверу')}</h2>
<div class="card pad">
  <h3 class="in-card">{t('Токен доступа')}</h3>
  <p class="hint">
    {t('Токен доступа есть в каждом коде приглашения и коде создания группы. Смените его, если код или устройство попали в чужие руки: старые коды перестанут работать, неиспользованные коды создания группы будут отозваны.')}
  </p>
  {#if confirming === 'relay'}
    <div class="confirm" role="alertdialog">
      <strong>{t('Сменить токен доступа?')}</strong>
      <p class="hint warn">
        {t('Все устройства всех групп на этом сервере сразу отключатся. Чтобы вернуться, каждое должно выйти из группы и подключиться по новому коду приглашения своей группы. Это устройство переподключится само.')}
      </p>
      <div class="buttons">
        <button class="danger" disabled={acting} onclick={act}>{acting ? t('Меняю…') : t('Сменить токен')}</button>
        <button disabled={acting} onclick={() => (confirming = null)}>{t('Отмена')}</button>
      </div>
    </div>
  {:else}
    <button class="danger" disabled={acting} onclick={() => ask('relay')}>{t('Сменить токен доступа')}</button>
  {/if}
  {#if relayResult}
    <div class="done" role="status">
      <p>{t('Токен сменён. Новый код приглашения в эту группу:')}</p>
      <code class="code selectable">{relayResult.invite}</code>
      <div class="buttons">
        <button onclick={() => copy(relayResult!.invite, 'invite')}>{copied === 'invite' ? t('Скопировано') : t('Скопировать код приглашения')}</button>
      </div>
      <p class="hint">{t('Новый код сервера администратора (с ним создают группы, храните его только у себя):')}</p>
      <code class="code selectable">{relayResult.server_code}</code>
      <div class="buttons">
        <button onclick={() => copy(relayResult!.server_code, 'relay-server')}>{copied === 'relay-server' ? t('Скопировано') : t('Скопировать код сервера')}</button>
        <button onclick={() => (relayResult = null)}>{t('Скрыть')}</button>
      </div>
      <p class="hint">{t('Старые коды приглашения других групп на этом сервере тоже перестали работать. Чтобы вернуть такую группу, введите новый код сервера на одном из её устройств («Синхронизация» → «Я администратор сервера»): оно переподключится и покажет новый код приглашения для остальных.')}</p>
    </div>
  {/if}
</div>

<div class="card pad">
  <h3 class="in-card">{t('Ключ администратора')}</h3>
  <p class="hint">
    {t('Ключ администратора есть только в коде сервера (cwserver2-…). С ним создают группы и меняют настройки сервера. Смените его, если код сервера мог увидеть кто-то ещё.')}
  </p>
  {#if confirming === 'admin'}
    <div class="confirm" role="alertdialog">
      <strong>{t('Сменить ключ администратора?')}</strong>
      <p class="hint">
        {t('Старый код сервера перестанет работать: другие устройства администратора потеряют раздел «Сервер», пока на них не введут новый код. Синхронизация ни у кого не прервётся.')}
      </p>
      <div class="buttons">
        <button class="danger" disabled={acting} onclick={act}>{acting ? t('Меняю…') : t('Сменить ключ')}</button>
        <button disabled={acting} onclick={() => (confirming = null)}>{t('Отмена')}</button>
      </div>
    </div>
  {:else}
    <button class="danger" disabled={acting} onclick={() => ask('admin')}>{t('Сменить ключ администратора')}</button>
  {/if}
  {#if adminCode}
    <div class="done" role="status">
      <p>{t('Ключ сменён. Новый код сервера администратора:')}</p>
      <code class="code selectable">{adminCode}</code>
      <div class="buttons">
        <button onclick={() => copy(adminCode, 'admin-server')}>{copied === 'admin-server' ? t('Скопировано') : t('Скопировать код сервера')}</button>
        <button onclick={() => (adminCode = '')}>{t('Скрыть')}</button>
      </div>
      <p class="hint">{t('На этом устройстве новый ключ уже сохранён. Сервер тоже печатает новый код в лог контейнера.')}</p>
    </div>
  {/if}
</div>

<div class="card pad">
  <h3 class="in-card">{t('Это устройство')}</h3>
  <p class="hint">{t('Ключ администратора хранится на этом устройстве зашифрованным. Если устройство больше не должно управлять сервером, забудьте ключ: синхронизация останется.')}</p>
  {#if confirming === 'forget'}
    <div class="confirm" role="alertdialog">
      <strong>{t('Забыть ключ администратора на этом устройстве?')}</strong>
      <div class="buttons">
        <button class="danger" disabled={acting} onclick={act}>{t('Забыть ключ')}</button>
        <button disabled={acting} onclick={() => (confirming = null)}>{t('Отмена')}</button>
      </div>
    </div>
  {:else}
    <button disabled={acting} onclick={() => ask('forget')}>{t('Забыть ключ администратора')}</button>
  {/if}
</div>
{#if actionError}<p class="error outside" role="alert">{actionError}</p>{/if}

<style>
  h2 {
    font-size: var(--text-s);
    font-weight: 400;
    color: var(--c-muted);
    margin: var(--space-5) 0 var(--space-2) 2px;
  }
  h3 {
    font-size: var(--text-s);
    font-weight: 400;
    color: var(--c-muted);
    margin: var(--space-4) 0 var(--space-2) 2px;
  }
  h3.in-card {
    margin: 0;
    font-size: var(--text-m, inherit);
    font-weight: 600;
    color: var(--c-text);
  }
  .section-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
  }
  .section-head button {
    align-self: baseline;
  }
  .card {
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-l);
    margin-bottom: var(--space-3);
  }
  .card.list {
    padding: 2px var(--space-4);
  }
  .card.list > :global(* + *) {
    border-top: 1px solid var(--c-border);
  }
  .card.pad {
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .kv {
    display: flex;
    justify-content: space-between;
    gap: var(--space-4);
    padding: 11px 0;
    margin: 0;
  }
  .kv > span:first-child {
    color: var(--c-muted);
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
  .accent-text {
    color: var(--c-accent);
  }
  .ok {
    color: var(--c-ok);
    font-size: var(--text-s);
  }
  .bad {
    font-size: var(--text-s);
    color: var(--c-danger);
  }
  .error {
    color: var(--c-danger);
    margin: 0;
  }
  .error.outside {
    margin: 0 2px var(--space-3);
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
  }
  .selectable {
    user-select: all;
  }
  .group {
    padding: 11px 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .group-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  /* Пустые и надгробные группы: место не занимают, внимания не требуют. */
  .group-row.muted .group-main {
    opacity: 0.55;
  }
  .group-main {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .group-title {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-2);
  }
  .group-meta {
    font-size: var(--text-s);
    color: var(--c-muted);
    overflow-wrap: anywhere;
  }
  .tag {
    font-size: var(--text-s);
    color: var(--c-muted);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 0 6px;
  }
  .tag.accent {
    color: var(--c-accent);
    border-color: var(--c-accent);
  }
  .group-row button {
    align-self: center;
    flex-shrink: 0;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }
  .field label {
    color: var(--c-text);
  }
  .input-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  input {
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 7px var(--space-3);
    color: var(--c-text);
    font: inherit;
  }
  input.wide {
    width: 100%;
    max-width: 40ch;
  }
  input.num {
    width: 14ch;
    font-family: var(--font-mono);
  }
  input.invalid {
    border-color: var(--c-danger);
  }
  .unit {
    color: var(--c-muted);
    font-size: var(--text-s);
  }
  .segmented {
    display: flex;
    gap: 2px;
    padding: 3px;
    background: var(--c-bg);
    border-radius: var(--radius-m);
  }
  .segmented button {
    border: 0;
    padding: 4px 10px;
    color: var(--c-muted);
    font-size: var(--text-s);
  }
  .segmented button.on {
    background: var(--c-surface);
    color: var(--c-text);
  }
  .save-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin: 0 0 var(--space-3);
  }
  /* Несохранённые пределы видны, даже если форма ушла вверх. */
  .save-bar.dirty {
    position: sticky;
    bottom: 0;
    padding: var(--space-3) var(--space-4);
    background: var(--c-surface);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-l);
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
  button.small {
    padding: 5px 10px;
    font-size: var(--text-s);
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
  button.link {
    border: 0;
    padding: 0;
    color: var(--c-accent);
    background: none;
  }
  button.link.small {
    font-size: var(--text-s);
  }
  button.link:hover:not(:disabled) {
    background: none;
    text-decoration: underline;
  }
</style>
