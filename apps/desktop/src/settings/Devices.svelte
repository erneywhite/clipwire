<script lang="ts">
  // Устройства группы: список, удаление устройства и смена ключа группы.
  import { onMount } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { api, type GroupDevice, type GroupRekeyed, type RekeyResult, type Status } from '../lib/api'
  import { ago, plural } from '../lib/format'
  import { CHECKING, confirmText, deviceTitle, seenLabel } from '../lib/devices'

  let { onRekeyed }: { onRekeyed?: (invite: string) => void } = $props()

  const fmt = { ago, plural }

  let list = $state<GroupDevice[]>([])
  let loaded = $state(false)
  let error = $state('')
  let busy = $state(false)
  /** Что подтверждаем: id удаляемого устройства, '' — смена ключа без удаления. */
  let confirming = $state<string | null>(null)
  let result = $state<RekeyResult | null>(null)
  let copied = $state(false)
  /** Ответ на смену ключа потерялся, ждём, что скажет сервер при повторном входе. */
  let checking = $state(false)

  const explain = $derived(confirming === null ? [] : confirmText(list, confirming || null, fmt))

  async function load() {
    try {
      list = await api.devicesList()
      error = ''
    } catch (e) {
      error = String(e)
    } finally {
      loaded = true
    }
  }

  async function run() {
    if (confirming === null) return
    busy = true
    error = ''
    checking = false
    try {
      const id = confirming
      done(id ? await api.deviceRemove(id) : await api.groupRekey())
    } catch (e) {
      const msg = String(e)
      if (msg.includes(CHECKING)) {
        checking = true
        confirming = null
      } else {
        error = msg
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

  onMount(() => {
    // Список обновляется, когда сервис снова на связи (в том числе после
    // смены ключа): раньше движок ответить не может.
    const unlisten = [
      listen<Status>('clipwire://status', (e) => {
        if (e.payload.kind === 'online') load()
      }),
      // Ключ сменили здесь, а ответ сервера потерялся: успех приходит
      // событием, когда устройство снова вошло в старую группу.
      listen<GroupRekeyed>('clipwire://group-rekeyed', (e) => {
        if (e.payload.by_me && e.payload.result) done(e.payload.result)
      }),
      listen<string>('clipwire://group-rekey-failed', (e) => {
        checking = false
        error = e.payload
      }),
    ]
    load()
    return () => unlisten.forEach((p) => p.then((f) => f()))
  })
</script>

<h2>Устройства группы</h2>
<div class="card">
  {#if !loaded}
    <p class="row hint">Загружаю список…</p>
  {:else if list.length === 0}
    <p class="row hint">{error ? 'Список недоступен, пока нет связи с сервером.' : 'В группе пока нет устройств.'}</p>
  {/if}
  {#each list as d (d.id)}
    <div class="row device">
      <span class="dot" class:on={d.online}></span>
      <div class="who">
        <span class="name" class:unknown={!d.known}>{deviceTitle(d)}{#if d.me}<span class="me"> · это устройство</span>{/if}</span>
        <span class="meta mono">{d.os ?? 'нет карточки'} · {d.fingerprint} · {seenLabel(d, fmt)}</span>
      </div>
      {#if !d.me}
        <button class="danger" disabled={busy} onclick={() => ((confirming = d.id), (result = null))}>Удалить</button>
      {/if}
    </div>
  {/each}
</div>
{#if list.some((d) => !d.known && !d.me)}
  <p class="hint outside">
    У неизвестных устройств старая версия Clipwire или карточка не открылась. При смене ключа новый ключ им передать
    нечем: обновите их и откройте на них Clipwire, тогда имя появится.
  </p>
{/if}

<div class="card pad">
  {#if confirming !== null}
    <div class="confirm">
      <strong>{confirming ? 'Удалить устройство и сменить ключ группы?' : 'Сменить ключ группы?'}</strong>
      {#each explain as line (line)}
        <p class="hint" class:warn={line.startsWith('Внимание')}>{line}</p>
      {/each}
      <div class="buttons">
        <button class="danger" disabled={busy} onclick={run}>
          {busy ? 'Меняю ключ…' : confirming ? 'Удалить устройство' : 'Сменить ключ'}
        </button>
        <button disabled={busy} onclick={() => (confirming = null)}>Отмена</button>
      </div>
    </div>
  {:else}
    <p class="hint">
      Новый ключ получат все устройства группы, а старый код приглашения перестанет работать. Пригодится, если код
      приглашения попал не туда.
    </p>
    <button disabled={busy || !loaded || list.length === 0} onclick={() => ((confirming = ''), (result = null))}>
      Сменить ключ группы
    </button>
  {/if}
  {#if checking}<p class="hint" role="status">{CHECKING}</p>{/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if result}
    <div class="done" role="status">
      <p>Ключ группы сменён. Новый код приглашения:</p>
      <code class="code selectable">{result.invite}</code>
      <div class="buttons">
        <button onclick={copyInvite}>{copied ? 'Скопировано' : 'Скопировать код'}</button>
        <button onclick={() => (result = null)}>Скрыть</button>
      </div>
      {#if result.skipped.length > 0}
        <p class="hint warn">
          Не получили новый ключ: {result.skipped.join(', ')}. Подключите их заново по этому коду.
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
  .meta {
    color: var(--c-muted);
    overflow-wrap: anywhere;
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
</style>
