<script lang="ts">
  // Хранилище файлов группы: сервер Clipwire, S3-совместимое или SFTP.
  import { onMount } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type ProbeResult, type StorageForm, type StorageKind, type StorageState, type StorageWarning, type Status } from '../lib/api'
  import {
    MAX_KEY_BYTES,
    copyForm,
    hostChanged,
    hostId,
    hostKeyChanged,
    isDirty,
    isHttp,
    keepsSecret,
    keyProblem,
    probeKey,
    ttlOptions,
    validate,
    warningTime,
  } from '../lib/storage'

  const kinds: { id: StorageKind; title: string; hint: string }[] = [
    { id: 'server', title: 'Сервер Clipwire', hint: 'Файлы лежат на сервере группы. Ничего настраивать не нужно.' },
    { id: 's3', title: 'S3-совместимое', hint: 'AWS S3, MinIO, Backblaze B2, Cloudflare R2 и другие.' },
    { id: 'sftp', title: 'SFTP', hint: 'Любой сервер с SSH: NAS, VPS, домашний компьютер.' },
  ]

  let stored = $state<StorageState | null>(null)
  let form = $state<StorageForm | null>(null)
  let loadError = $state('')
  let error = $state('')
  let saving = $state(false)
  let savedNote = $state(false)
  /** Настройки изменили на другом устройстве, пока здесь шла правка. */
  let remoteChanged = $state(false)
  let warning = $state<StorageWarning | null>(null)
  /** Подтвердить сохранение без успешной проверки. */
  let confirmUnchecked = $state(false)

  // Проверка подключения относится к настройкам, на которых её запускали.
  let probe = $state<ProbeResult | null>(null)
  let probing = $state(false)
  let probedFor = $state('')

  // Отпечаток ключа хоста: какой прислал сервер и для какого хоста и порта.
  let offeredKey = $state('')
  let keyFor = $state('')

  let showSecret = $state(false)
  let showPassword = $state(false)
  let showPassphrase = $state(false)
  let keyFile: HTMLInputElement | undefined = $state()

  const savedForm = $derived(stored?.form ?? null)
  const errors = $derived(form && savedForm && stored ? validate(form, savedForm, stored.saved) : {})
  const valid = $derived(Object.keys(errors).length === 0)
  const dirty = $derived(!!form && !!savedForm && isDirty(form, savedForm))
  const keep = $derived(!!form && !!savedForm && !!stored && keepsSecret(form, savedForm, stored.saved))
  const probeFresh = $derived(!!form && probedFor === probeKey(form))
  const probeOk = $derived(probeFresh && probe?.status === 'ok')
  const needsKeyConfirm = $derived(form?.kind === 'sftp' && !form.sftp.host_key_sha256)
  /** Подтверждён другой ключ сервера: сохранённый пароль или ключ ему не отправляются. */
  const rekeyedHost = $derived(!!form && !!savedForm && hostKeyChanged(form, savedForm))

  function apply(s: StorageState) {
    stored = s
    form = copyForm(s.form)
    warning = s.warning
    keyFor = hostId(s.form.sftp)
    offeredKey = ''
    probe = null
    probedFor = ''
    remoteChanged = false
    confirmUnchecked = false
  }

  async function load() {
    try {
      apply(await api.storageGet())
      loadError = ''
    } catch (e) {
      loadError = String(e)
    }
  }

  // Сменился хост или порт SFTP — подтверждённый отпечаток к нему не относится.
  $effect(() => {
    if (!form) return
    if (hostChanged(form.sftp, keyFor)) {
      form.sftp.host_key_sha256 = ''
      offeredKey = ''
      keyFor = hostId(form.sftp)
    }
  })

  async function runProbe() {
    if (!form || !valid) return
    probing = true
    error = ''
    confirmUnchecked = false
    const snapshot = copyForm(form)
    try {
      const r = await api.storageProbe(snapshot)
      probe = r
      probedFor = probeKey(snapshot)
      if (r.status === 'confirm_host_key') offeredKey = r.host_key
    } catch (e) {
      probe = { status: 'error', message: String(e) }
      probedFor = probeKey(snapshot)
    } finally {
      probing = false
    }
  }

  /**
   * «Отпечаток совпадает»: запомнить его и сразу проверить вход и запись.
   * Если подтверждён не тот ключ, что сохранён, сохранённый пароль или ключ
   * не подставляется: проверка ждёт, пока его введут заново.
   */
  function confirmKey() {
    if (!form || !offeredKey) return
    form.sftp.host_key_sha256 = offeredKey
    keyFor = hostId(form.sftp)
    offeredKey = ''
    if (valid) runProbe()
  }

  /** Сервер сменил ключ: забыть старый отпечаток и показать новый на подтверждение. */
  function resetKey() {
    if (!form) return
    form.sftp.host_key_sha256 = ''
    probe = null
    probedFor = ''
    runProbe()
  }

  async function save(force = false) {
    if (!form || !valid) return
    if (form.kind !== 'server' && !probeOk && !force) {
      confirmUnchecked = true
      return
    }
    saving = true
    error = ''
    confirmUnchecked = false
    try {
      const wasOk = probeOk
      const s = await api.storageSave(copyForm(form))
      const p = probe
      const pf = probedFor
      apply(s)
      // Проверка по только что сохранённому остаётся в силе.
      if (wasOk) {
        probe = p
        probedFor = pf
      }
      savedNote = true
      setTimeout(() => (savedNote = false), 2500)
    } catch (e) {
      error = String(e)
    } finally {
      saving = false
    }
  }

  function revert() {
    if (stored) apply(stored)
  }

  async function pickKey(e: Event) {
    const input = e.currentTarget as HTMLInputElement
    const file = input.files?.[0]
    input.value = ''
    if (!file || !form) return
    if (file.size > MAX_KEY_BYTES) {
      error = 'Файл слишком большой для закрытого ключа'
      return
    }
    const text = await file.text()
    const problem = keyProblem(text)
    if (problem) {
      error = `${file.name}: ${problem}`
      return
    }
    error = ''
    form.sftp.private_key = text
  }

  function kindHint(id: StorageKind) {
    return kinds.find((k) => k.id === id)?.hint ?? ''
  }

  onMount(() => {
    const unlisten = [
      listen<Status>('clipwire://status', (e) => {
        if (e.payload.kind === 'online' && !stored) load()
      }),
      // Настройки сохранили на другом устройстве (или здесь, ответом сервера).
      listen('clipwire://group-config', () => {
        if (dirty) remoteChanged = true
        else load()
      }),
      listen<StorageWarning>('clipwire://storage-warning', (e) => (warning = e.payload)),
    ]
    load()
    return () => unlisten.forEach((p) => p.then((f) => f()))
  })
</script>

<h2>Хранилище файлов</h2>
<div class="card pad">
  {#if !form || !stored}
    <p class="hint">{loadError || 'Загружаю настройки…'}</p>
  {:else}
    <div class="segmented" role="radiogroup" aria-label="Где хранить файлы">
      {#each kinds as k (k.id)}
        <button role="radio" aria-checked={form.kind === k.id} class:on={form.kind === k.id} onclick={() => (form!.kind = k.id)}>{k.title}</button>
      {/each}
    </div>
    <p class="hint">{kindHint(form.kind)}</p>

    {#if form.kind === 's3'}
      <div class="grid">
        <label class="field wide">
          <span>Адрес API</span>
          <input bind:value={form.s3.endpoint} placeholder="https://s3.eu-central-1.amazonaws.com" spellcheck="false" autocomplete="off" />
          {#if errors.endpoint && form.s3.endpoint}<span class="bad">{errors.endpoint}</span>{/if}
        </label>
        <label class="field">
          <span>Регион</span>
          <input bind:value={form.s3.region} placeholder="us-east-1" spellcheck="false" autocomplete="off" />
        </label>
        <label class="field">
          <span>Бакет</span>
          <input bind:value={form.s3.bucket} placeholder="clipwire" spellcheck="false" autocomplete="off" />
          {#if errors.bucket && form.s3.bucket}<span class="bad">{errors.bucket}</span>{/if}
        </label>
        <label class="field wide">
          <span>Папка в бакете (можно оставить пустой)</span>
          <input bind:value={form.s3.prefix} placeholder="clipwire/files" spellcheck="false" autocomplete="off" />
        </label>
        <label class="field">
          <span>Ключ доступа</span>
          <input bind:value={form.s3.access_key} class="mono" spellcheck="false" autocomplete="off" />
        </label>
        <label class="field">
          <span>Секретный ключ</span>
          <span class="secret">
            <input
              type={showSecret ? 'text' : 'password'}
              bind:value={form.s3.secret_key}
              class="mono"
              placeholder={keep ? 'Сохранён, оставьте пустым' : ''}
              spellcheck="false"
              autocomplete="off"
            />
            <button type="button" class="eye" onclick={() => (showSecret = !showSecret)} aria-pressed={showSecret}>{showSecret ? 'Скрыть' : 'Показать'}</button>
          </span>
        </label>
      </div>
      <label class="check">
        <input type="checkbox" bind:checked={form.s3.path_style} />
        <span>
          Адресация через путь
          <span class="hint">Адрес вида <code>адрес/бакет/файл</code>. Нужна MinIO и большинству совместимых хранилищ; у AWS можно выключить.</span>
        </span>
      </label>
      {#if isHttp(form.s3.endpoint) && !errors.endpoint}
        <p class="hint warn">
          Адрес без HTTPS: ключ доступа и подпись запросов идут открытым текстом. Подходит только для своей локальной
          сети. Сами файлы всё равно зашифрованы ключом группы.
        </p>
      {/if}
      <p class="hint">
        Устройства сами удаляют старые файлы по сроку ниже. Надёжнее добавить ещё и правило жизненного цикла в
        бакете (Lifecycle rule) на тот же срок: тогда файлы удалятся, даже если все устройства выключены.
      </p>
    {:else if form.kind === 'sftp'}
      <div class="grid">
        <label class="field wide">
          <span>Сервер</span>
          <input bind:value={form.sftp.host} placeholder="nas.example.com" spellcheck="false" autocomplete="off" />
          {#if errors.host && form.sftp.host}<span class="bad">{errors.host}</span>{/if}
        </label>
        <label class="field">
          <span>Порт</span>
          <input class="num" type="number" min="1" max="65535" step="1" bind:value={form.sftp.port} />
          {#if errors.port}<span class="bad">{errors.port}</span>{/if}
        </label>
        <label class="field">
          <span>Пользователь</span>
          <input bind:value={form.sftp.user} spellcheck="false" autocomplete="off" />
        </label>
        <label class="field wide">
          <span>Папка на сервере</span>
          <input bind:value={form.sftp.path} placeholder="/volume1/clipwire" spellcheck="false" autocomplete="off" />
        </label>
      </div>

      <div class="segmented small" role="radiogroup" aria-label="Способ входа">
        {#each [['password', 'Пароль'], ['key', 'Закрытый ключ']] as [id, title] (id)}
          <button role="radio" aria-checked={form.sftp.auth === id} class:on={form.sftp.auth === id} onclick={() => (form!.sftp.auth = id as 'password' | 'key')}>{title}</button>
        {/each}
      </div>
      {#if form.sftp.auth === 'password'}
        <label class="field">
          <span>Пароль</span>
          <span class="secret">
            <input
              type={showPassword ? 'text' : 'password'}
              bind:value={form.sftp.password}
              placeholder={keep ? 'Сохранён, оставьте пустым' : ''}
              autocomplete="off"
            />
            <button type="button" class="eye" onclick={() => (showPassword = !showPassword)} aria-pressed={showPassword}>{showPassword ? 'Скрыть' : 'Показать'}</button>
          </span>
          {#if errors.password && rekeyedHost}<span class="bad">{errors.password}</span>{/if}
        </label>
      {:else}
        <label class="field">
          <span>Закрытый ключ OpenSSH</span>
          <textarea
            bind:value={form.sftp.private_key}
            rows="4"
            placeholder={keep ? 'Сохранён, оставьте пустым, чтобы не менять' : '-----BEGIN OPENSSH PRIVATE KEY-----'}
            spellcheck="false"
            autocomplete="off"
          ></textarea>
          {#if errors.private_key && (form.sftp.private_key.trim() || rekeyedHost)}<span class="bad">{errors.private_key}</span>{/if}
        </label>
        <div class="buttons">
          <button type="button" onclick={() => keyFile?.click()}>Выбрать файл ключа…</button>
          <input bind:this={keyFile} type="file" hidden onchange={pickKey} />
        </div>
        <label class="field">
          <span>Пароль к ключу, если есть</span>
          <span class="secret">
            <input
              type={showPassphrase ? 'text' : 'password'}
              bind:value={form.sftp.passphrase}
              placeholder={keep && stored.saved.sftp_passphrase ? 'Сохранён, оставьте пустым' : ''}
              autocomplete="off"
            />
            <button type="button" class="eye" onclick={() => (showPassphrase = !showPassphrase)} aria-pressed={showPassphrase}>{showPassphrase ? 'Скрыть' : 'Показать'}</button>
          </span>
        </label>
      {/if}

      <div class="hostkey">
        <span class="label">Ключ сервера</span>
        {#if form.sftp.host_key_sha256}
          <code class="mono selectable">{form.sftp.host_key_sha256}</code>
          <span class="hint">Подтверждён. Устройства группы подключаются только к серверу с этим ключом.</span>
        {:else if offeredKey}
          <code class="mono selectable">{offeredKey}</code>
          <span class="hint">
            Сверьте отпечаток с сервером: на нём выполните <code>ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub</code>
            или посмотрите в панели NAS. Не совпадает — не подтверждайте: между вами и сервером кто-то есть.
          </span>
          <label class="check">
            <input type="checkbox" checked={false} onchange={(e) => e.currentTarget.checked && confirmKey()} />
            <span>Отпечаток совпадает</span>
          </label>
        {:else}
          <span class="hint">Ещё не подтверждён. Нажмите «Проверить подключение», чтобы увидеть отпечаток.</span>
        {/if}
      </div>
    {/if}

    {#if form.kind !== 'server'}
      <label class="field">
        <span>Сколько хранить файлы</span>
        <select bind:value={form.file_ttl_hours}>
          {#each ttlOptions(form.file_ttl_hours) as o (o.hours)}<option value={o.hours}>{o.title}</option>{/each}
        </select>
      </label>
      <p class="hint">
        Настройки, включая ключи доступа, получат все устройства группы. Сервер Clipwire их не видит: они зашифрованы
        ключом группы, как и сами файлы.
      </p>
    {/if}

    {#if stored.access_exposed && form.kind !== 'server'}
      <p class="hint warn" role="alert">
        Из группы исключили устройство, у которого остались эти доступы к хранилищу файлов.
        {stored.form.kind === 'sftp'
          ? 'Смените пароль или ключ SSH на сервере и сохраните новые здесь.'
          : 'Смените ключ S3 у поставщика хранилища и сохраните новый здесь.'}
        Пока доступы прежние, исключённое устройство может удалять и портить файлы группы в хранилище, хотя прочитать их не может.
      </p>
    {/if}
    {#if stored.error && !dirty}
      <p class="error" role="alert">Сохранённое хранилище не открывается: {stored.error}. Файлы пока идут через сервер Clipwire.</p>
    {/if}
    {#if remoteChanged}
      <p class="hint warn" role="status">
        Настройки хранилища изменили на другом устройстве.
        <button class="link" onclick={load}>Показать новые</button> (ваши правки пропадут)
      </p>
    {/if}

    {#if probing}
      <p class="hint" role="status">Проверяю подключение…</p>
    {:else if probe && probeFresh}
      {#if probe.status === 'ok'}
        <p class="ok" role="status">Подключение работает: запись, чтение и удаление прошли.</p>
      {:else if probe.status === 'confirm_host_key'}
        {#if !form.sftp.host_key_sha256}<p class="hint" role="status">Сервер ответил. Сверьте отпечаток его ключа выше.</p>{/if}
      {:else if probe.status === 'host_key_mismatch'}
        <div class="error" role="alert">
          <p>Сервер прислал другой ключ: <code class="selectable">{probe.got}</code>, а подтверждён <code class="selectable">{probe.expected}</code>.</p>
          <p class="hint">
            Смена ключа сервера может значить, что соединение перехватили. Если ключ правда меняли (переустановка,
            новый сервер), сбросьте отпечаток и подтвердите новый, а потом введите пароль или ключ заново: сохранённый
            серверу с другим ключом не отправляется. Если не меняли, ничего не трогайте.
          </p>
          <button class="danger" onclick={resetKey}>Сбросить отпечаток</button>
        </div>
      {:else}
        <p class="error" role="alert">Не подключилось: {probe.message}</p>
      {/if}
    {/if}

    {#if confirmUnchecked}
      <div class="confirm">
        <span>Подключение не проверено. Если настройки неверны, файлы не дойдут через хранилище, и устройства будут слать их через сервер Clipwire. Сохранить всё равно?</span>
        <button class="danger" disabled={saving} onclick={() => save(true)}>Сохранить без проверки</button>
        <button onclick={() => (confirmUnchecked = false)}>Отмена</button>
      </div>
    {/if}

    <div class="buttons">
      {#if form.kind !== 'server'}
        <button disabled={probing || !valid} onclick={runProbe}>{probing ? 'Проверяю…' : 'Проверить подключение'}</button>
      {/if}
      <button class="primary" disabled={saving || probing || !dirty || !valid || needsKeyConfirm} onclick={() => save()}>
        {saving ? 'Сохраняю…' : 'Сохранить'}
      </button>
      {#if dirty}<button disabled={saving} onclick={revert}>Отменить правки</button>{/if}
      {#if savedNote}<span class="ok" role="status">Сохранено</span>{/if}
    </div>
    {#if needsKeyConfirm && valid}
      <p class="hint">Сохранить можно после того, как вы подтвердите отпечаток ключа сервера.</p>
    {/if}
    {#if error}<p class="error" role="alert">{error}</p>{/if}

    {#if warning}
      <div class="warning" role="status">
        <span>
          <span class="mono">{warningTime(warning.at_ms)}</span>
          Хранилище группы не ответило, файл ушёл через сервер Clipwire: {warning.text}
        </span>
        <button onclick={() => (warning = null)}>Скрыть</button>
      </div>
    {/if}
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
    padding: var(--space-4);
    margin-bottom: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: 0;
  }
  .hint.warn {
    color: var(--c-danger);
  }
  .ok {
    color: var(--c-ok);
    margin: 0;
    font-size: var(--text-s);
  }
  .error {
    color: var(--c-danger);
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .error p {
    margin: 0;
  }
  .mono,
  code {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .selectable {
    user-select: all;
    overflow-wrap: anywhere;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: var(--space-3);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }
  .field.wide {
    grid-column: 1 / -1;
  }
  .field > span:first-child,
  .hostkey .label {
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .bad {
    font-size: var(--text-s);
    color: var(--c-danger);
  }
  input:not([type='checkbox']),
  textarea,
  select {
    width: 100%;
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 8px var(--space-3);
    color: var(--c-text);
    font: inherit;
  }
  textarea {
    font-family: var(--font-mono);
    font-size: var(--text-s);
    resize: vertical;
  }
  select {
    width: auto;
    min-width: 16ch;
    align-self: flex-start;
    cursor: pointer;
  }
  input.num {
    width: 10ch;
    font-family: var(--font-mono);
  }
  input.mono {
    font-family: var(--font-mono);
  }
  .secret {
    display: flex;
    gap: var(--space-2);
  }
  .secret input {
    flex: 1;
    min-width: 0;
  }
  .check {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    cursor: pointer;
  }
  .check > span {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .check input {
    margin-top: 3px;
    accent-color: var(--c-accent);
  }
  .hostkey {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--c-bg);
    border-radius: var(--radius-m);
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
  .segmented.small {
    align-self: flex-start;
  }
  .confirm,
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .confirm span {
    flex: 1 1 100%;
  }
  .warning {
    display: flex;
    gap: var(--space-3);
    align-items: flex-start;
    justify-content: space-between;
    padding: var(--space-3);
    border: 1px solid var(--c-danger);
    border-radius: var(--radius-m);
    font-size: var(--text-s);
  }
  .warning .mono {
    color: var(--c-muted);
    margin-right: var(--space-1);
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
  button.eye {
    flex: none;
    padding: 7px 10px;
    font-size: var(--text-s);
  }
  button.link {
    border: 0;
    padding: 0;
    color: var(--c-accent);
    text-decoration: underline;
    font-size: inherit;
  }
  button.link:hover:not(:disabled) {
    background: none;
  }
  @media (max-width: 560px) {
    .grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
