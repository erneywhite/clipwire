<script lang="ts">
  // Таблица правил исключений. Строки показываются текстом: условие,
  // значение, действие плашкой; править — по значку, прямо в строке, одна
  // строка за раз. Любая правка сразу сохраняется (`onchange` со всем
  // списком).
  import { api, type ExclusionRule, type RuleWhat } from '../lib/api'
  import { t } from '../lib/i18n'

  let { rules, onchange }: { rules: ExclusionRule[]; onchange: (next: ExclusionRule[]) => void } = $props()

  const whats: { id: RuleWhat; title: string }[] = [
    { id: 'ext', title: t('Расширение') },
    { id: 'name_starts', title: t('Имя начинается с') },
    { id: 'name_contains', title: t('Имя содержит') },
    { id: 'name_glob', title: t('Имя по маске') },
    { id: 'size_over', title: t('Размер больше') },
    { id: 'size_under', title: t('Размер меньше') },
  ]
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

  const isSize = (w: RuleWhat) => w === 'size_over' || w === 'size_under'
  const whatTitle = (w: RuleWhat) => whats.find((x) => x.id === w)?.title ?? w

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
    <span role="columnheader">{t('Правило')}</span>
    <span role="columnheader">{t('Действие')}</span>
    <span></span>
  </div>
  {#each rules as r, i (i)}
    {#if editing === i}
      <div class="row edit" role="row">
        <input
          type="checkbox"
          checked={r.enabled}
          aria-label={t('Правило включено')}
          onchange={(e) => patch(i, { enabled: e.currentTarget.checked })}
        />
        <div class="editor">
          <select value={r.what} aria-label={t('Условие')} onchange={(e) => setWhat(i, e.currentTarget.value as RuleWhat)}>
            {#each whats as w (w.id)}
              <option value={w.id}>{w.title}</option>
            {/each}
          </select>
          <select value={r.action} aria-label={t('Действие')} onchange={(e) => patch(i, { action: e.currentTarget.value as ExclusionRule['action'] })}>
            <option value="no_send">{actions.no_send}</option>
            <option value="no_upload">{actions.no_upload}</option>
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
          <button class="done" onclick={() => (editing = null)}>{t('Готово')}</button>
        </div>
      </div>
    {:else}
      <div class="row" class:off={!r.enabled} role="row">
        <input
          type="checkbox"
          checked={r.enabled}
          aria-label={t('Правило включено')}
          onchange={(e) => patch(i, { enabled: e.currentTarget.checked })}
        />
        <span class="rule">
          <span class="what">{whatTitle(r.what)}</span>
          <span class="value mono" title={valueText(r)}>{valueText(r)}</span>
        </span>
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
    grid-template-columns: 18px minmax(0, 1fr) 9.5rem 52px;
    gap: var(--space-3);
    align-items: center;
    padding: 8px 0;
    font-size: var(--text-m);
  }
  .row + .row {
    border-top: 1px solid var(--c-border);
  }
  .head {
    padding-top: 0;
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .row.off .rule,
  .row.off .pill {
    opacity: 0.5;
  }
  .row.edit {
    grid-template-columns: 18px minmax(0, 1fr);
    align-items: start;
  }
  .row.edit > input[type='checkbox'] {
    margin-top: 9px;
  }
  .editor {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) auto;
    gap: var(--space-2);
  }
  .editor > :nth-child(1) {
    grid-column: 1;
  }
  .editor > :nth-child(2) {
    grid-column: 2 / 4;
  }
  .editor > :nth-child(3) {
    grid-column: 1 / 3;
  }
  .rule {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .what {
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  /* Длинный список масок — до двух строк, целиком в подсказке. */
  .value {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    overflow: hidden;
    overflow-wrap: anywhere;
  }
  .row > :nth-child(3) {
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
  input[type='checkbox'] {
    margin: 0;
    accent-color: var(--c-accent);
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
  .done {
    padding: 6px 12px;
  }
</style>
