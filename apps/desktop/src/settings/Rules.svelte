<script lang="ts">
  // Таблица правил исключений. Строки показываются текстом: условие,
  // значение, действие плашкой; править — по значку, прямо в строке, одна
  // строка за раз. Любая правка сразу сохраняется (`onchange` со всем
  // списком).
  import { api, type ExclusionRule, type RuleWhat } from '../lib/api'
  import { t } from '../lib/i18n'

  let { rules, onchange }: { rules: ExclusionRule[]; onchange: (next: ExclusionRule[]) => void } = $props()

  const placeholders: Record<RuleWhat, string> = {
    ext: 'mp4, mkv, iso',
    name_starts: '~$',
    name_contains: t('черновик'),
    name_glob: '*.tmp, Thumbs.db',
    size_over: '',
    size_under: '',
  }
  const KB = 1024
  const units = [
    { id: 'kb', title: t('КБ'), bytes: KB },
    { id: 'mb', title: t('МБ'), bytes: KB * KB },
    { id: 'gb', title: t('ГБ'), bytes: KB * KB * KB },
  ]
  const actions = { no_send: t('Не отправлять'), no_upload: t('Не загружать сразу') }

  type Kind = 'ext' | 'name' | 'size'
  const kinds: { id: Kind; title: string }[] = [
    { id: 'ext', title: t('Расширение') },
    { id: 'name', title: t('Имя') },
    { id: 'size', title: t('Размер') },
  ]
  const conds: Record<Kind, { id: RuleWhat; title: string }[]> = {
    ext: [{ id: 'ext', title: t('одно из') }],
    name: [
      { id: 'name_starts', title: t('начинается с') },
      { id: 'name_contains', title: t('содержит') },
      { id: 'name_glob', title: t('по маске') },
    ],
    size: [
      { id: 'size_over', title: t('больше') },
      { id: 'size_under', title: t('меньше') },
    ],
  }
  const kindOf = (w: RuleWhat): Kind => (w === 'ext' ? 'ext' : w.startsWith('name') ? 'name' : 'size')
  const kindTitle = (w: RuleWhat) => kinds.find((k) => k.id === kindOf(w))?.title ?? w
  const condTitle = (w: RuleWhat) => conds[kindOf(w)].find((c) => c.id === w)?.title ?? w

  const isSize = (w: RuleWhat) => w === 'size_over' || w === 'size_under'

  /** Самая крупная единица, в которой размер целый. */
  function unitOf(size: number) {
    return [...units].reverse().find((u) => size >= u.bytes && size % u.bytes === 0) ?? units[1]
  }

  function valueText(r: ExclusionRule): string {
    if (!isSize(r.what)) return r.value.trim() || '—'
    const u = unitOf(r.size)
    return `${r.size / u.bytes} ${u.title}`
  }

  /** Какая строка открыта для правки; `null` — ни одна. */
  let editing = $state<number | null>(null)

  function patch(i: number, p: Partial<ExclusionRule>) {
    onchange(rules.map((r, j) => (j === i ? { ...r, ...p } : r)))
  }

  function setWhat(i: number, what: RuleWhat) {
    // Между текстом и размером значение не переносится: «mp4» — не размер.
    const was = rules[i]
    const keep = isSize(what) === isSize(was.what)
    patch(i, { what, value: keep ? was.value : '', size: keep ? was.size : isSize(what) ? 100 * KB * KB : 0 })
  }

  function setSize(i: number, n: number, unit: string) {
    const u = units.find((x) => x.id === unit) ?? units[1]
    patch(i, { size: Math.max(0, Math.round(n * u.bytes)) })
  }

  function add() {
    onchange([...rules, { enabled: true, what: 'ext', value: '', size: 0, action: 'no_upload' }])
    editing = rules.length
  }

  function remove(i: number) {
    onchange(rules.filter((_, j) => j !== i))
    if (editing === i) editing = null
    else if (editing !== null && editing > i) editing -= 1
  }

  /** Вернуть правила по умолчанию, которых нет в списке; свои не трогаются. */
  async function restore() {
    const defs = await api.exclusionDefaults()
    const same = (a: ExclusionRule, b: ExclusionRule) => a.what === b.what && a.value === b.value && a.action === b.action
    const missing = defs.filter((d) => !rules.some((r) => same(r, d)))
    if (missing.length) onchange([...rules, ...missing])
  }
</script>

<div class="table" role="table" aria-label={t('Правила для файлов и папок')}>
  <div class="row head" role="row">
    <span></span>
    <span role="columnheader">{t('Что')}</span>
    <span role="columnheader">{t('Условие')}</span>
    <span role="columnheader">{t('Значение')}</span>
    <span role="columnheader">{t('Действие')}</span>
    <span></span>
  </div>
  {#each rules as r, i (i)}
    {#if editing === i}
      <div class="row edit" role="row">
        <input
          type="checkbox"
          role="switch"
          class="switch"
          checked={r.enabled}
          aria-label={t('Правило включено')}
          onchange={(e) => patch(i, { enabled: e.currentTarget.checked })}
        />
        <select value={kindOf(r.what)} aria-label={t('Что')} onchange={(e) => setWhat(i, conds[e.currentTarget.value as Kind][0].id)}>
          {#each kinds as k (k.id)}
            <option value={k.id}>{k.title}</option>
          {/each}
        </select>
        <select value={r.what} aria-label={t('Условие')} onchange={(e) => setWhat(i, e.currentTarget.value as RuleWhat)}>
          {#each conds[kindOf(r.what)] as c (c.id)}
            <option value={c.id}>{c.title}</option>
          {/each}
        </select>
        {#if isSize(r.what)}
          {@const u = unitOf(r.size)}
          <div class="size">
            <input
              class="mono"
              type="number"
              min="0"
              step="1"
              value={r.size / u.bytes}
              aria-label={t('Размер')}
              onchange={(e) => setSize(i, Number(e.currentTarget.value), u.id)}
            />
            <select value={u.id} aria-label={t('Единица')} onchange={(e) => setSize(i, r.size / u.bytes, e.currentTarget.value)}>
              {#each units as x (x.id)}
                <option value={x.id}>{x.title}</option>
              {/each}
            </select>
          </div>
        {:else}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="mono"
            value={r.value}
            placeholder={placeholders[r.what]}
            spellcheck="false"
            autofocus
            aria-label={t('Значение')}
            onchange={(e) => patch(i, { value: e.currentTarget.value })}
            onkeydown={(e) => e.key === 'Enter' && e.currentTarget.blur()}
          />
        {/if}
        <select value={r.action} aria-label={t('Действие')} onchange={(e) => patch(i, { action: e.currentTarget.value as ExclusionRule['action'] })}>
          <option value="no_send">{actions.no_send}</option>
          <option value="no_upload">{actions.no_upload}</option>
        </select>
        <span class="icons">
          <button class="icon ok" aria-label={t('Готово')} title={t('Готово')} onclick={() => (editing = null)}>
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12l5 5L19 7" /></svg>
          </button>
        </span>
      </div>
    {:else}
      <div class="row" class:off={!r.enabled} role="row">
        <input
          type="checkbox"
          role="switch"
          class="switch"
          checked={r.enabled}
          aria-label={t('Правило включено')}
          onchange={(e) => patch(i, { enabled: e.currentTarget.checked })}
        />
        <span class="cell">{kindTitle(r.what)}</span>
        <span class="cell muted">{condTitle(r.what)}</span>
        <span class="cell value mono" title={valueText(r)}>{valueText(r)}</span>
        <span><span class="pill" data-action={r.action}>{actions[r.action]}</span></span>
        <span class="icons">
          <button class="icon" aria-label={t('Изменить правило')} title={t('Изменить правило')} onclick={() => (editing = i)}>
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 20h4L19 9l-4-4L4 16zM14 6l4 4" /></svg>
          </button>
          <button class="icon" aria-label={t('Удалить правило')} title={t('Удалить правило')} onclick={() => remove(i)}>
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18" /></svg>
          </button>
        </span>
      </div>
    {/if}
  {:else}
    <p class="empty">{t('Правил нет')}</p>
  {/each}
</div>
<div class="buttons">
  <button onclick={restore}>{t('Вернуть правила по умолчанию')}</button>
  <button onclick={add}>+ {t('Добавить правило')}</button>
</div>

<style>
  .table {
    display: flex;
    flex-direction: column;
  }
  .row {
    display: grid;
    grid-template-columns: 28px 6.5rem 7.5rem minmax(0, 1fr) 9.5rem 52px;
    gap: var(--space-3);
    align-items: center;
    padding: 9px 0;
    font-size: var(--text-m);
  }
  .row + .row {
    border-top: 1px solid var(--c-border);
  }
  .head {
    padding-top: 2px;
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .row.edit {
    grid-template-columns: 28px 6.5rem 7.5rem minmax(0, 1fr) 10.5rem 52px;
    gap: var(--space-2);
    padding: 6px 0;
  }
  .row.off .cell,
  .row.off .pill {
    opacity: 0.5;
  }
  .cell {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .muted {
    color: var(--c-muted);
  }
  /* Длинный список масок — до двух строк, целиком в подсказке. */
  .cell.value {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    white-space: normal;
    /* Маски переносятся после запятой, а не посреди имени. */
    overflow-wrap: break-word;
  }
  .row > :nth-child(5) {
    justify-self: start;
  }
  .pill {
    display: inline-block;
    padding: 2px 8px;
    border-radius: var(--radius-s);
    font-size: var(--text-s);
    white-space: nowrap;
  }
  .pill[data-action='no_send'] {
    color: var(--c-danger);
    background: color-mix(in srgb, var(--c-danger) 14%, transparent);
  }
  .pill[data-action='no_upload'] {
    color: var(--c-accent);
    background: color-mix(in srgb, var(--c-accent) 14%, transparent);
  }
  .icons {
    display: flex;
    justify-content: flex-end;
    gap: 2px;
  }
  .icon {
    border: 0;
    padding: 4px;
    border-radius: var(--radius-s);
    color: var(--c-muted);
    display: grid;
    place-items: center;
  }
  .icon svg {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.6;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .icon:hover {
    color: var(--c-text);
  }
  .icon.ok {
    color: var(--c-accent);
  }
  .size {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: var(--space-1);
  }
  input:not([type='checkbox']),
  select {
    width: 100%;
    min-width: 0;
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 6px var(--space-2);
    color: var(--c-text);
    font: inherit;
  }
  .size select {
    width: auto;
  }
  /* Тумблер, как в остальных настройках, только меньше. */
  .switch {
    appearance: none;
    width: 28px;
    height: 16px;
    border-radius: 8px;
    background: var(--c-border);
    position: relative;
    margin: 0;
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .switch::after {
    content: '';
    position: absolute;
    top: 2px;
    left: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--c-muted);
    transition: transform var(--dur-fast) var(--ease-out), background var(--dur-fast) var(--ease-out);
  }
  .switch:checked {
    background: var(--c-accent);
  }
  .switch:checked::after {
    transform: translateX(12px);
    background: var(--c-on-accent);
  }
  .switch:focus-visible {
    outline: 2px solid var(--c-accent);
    outline-offset: 2px;
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .empty {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: var(--space-2) 0;
  }
  .buttons {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    flex-wrap: wrap;
    margin-top: var(--space-3);
  }
  button {
    border: 1px solid var(--c-border);
    background: transparent;
    border-radius: var(--radius-m);
    padding: 7px 14px;
    cursor: pointer;
    white-space: nowrap;
    color: inherit;
    font: inherit;
    transition: background var(--dur-fast) var(--ease-out);
  }
  button:hover {
    background: var(--c-hover);
  }
</style>
