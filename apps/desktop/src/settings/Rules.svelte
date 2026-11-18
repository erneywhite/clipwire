<script lang="ts">
  // Таблица правил исключений: условие, значение, действие, включено ли.
  // Каждая правка сразу сохраняется (`onchange` со всем списком). Значения
  // текстом сохраняются по выходу из поля, а не на каждую букву.
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

  const isSize = (w: RuleWhat) => w === 'size_over' || w === 'size_under'

  /** Самая крупная единица, в которой размер целый. */
  function unitOf(size: number) {
    return [...units].reverse().find((u) => size >= u.bytes && size % u.bytes === 0) ?? units[1]
  }

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
  }

  /** Вернуть правила по умолчанию, которых нет в списке; свои не трогаются. */
  async function restore() {
    const defs = await api.exclusionDefaults()
    const same = (a: ExclusionRule, b: ExclusionRule) => a.what === b.what && a.value === b.value && a.action === b.action
    const missing = defs.filter((d) => !rules.some((r) => same(r, d)))
    if (missing.length) onchange([...rules, ...missing])
  }
</script>

<div class="rules">
  {#each rules as r, i (i)}
    <div class="rule" class:off={!r.enabled}>
      <input
        type="checkbox"
        checked={r.enabled}
        aria-label={t('Правило включено')}
        onchange={(e) => patch(i, { enabled: e.currentTarget.checked })}
      />
      <select value={r.what} aria-label={t('Условие')} onchange={(e) => setWhat(i, e.currentTarget.value as RuleWhat)}>
        {#each whats as w (w.id)}
          <option value={w.id}>{w.title}</option>
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
        <input
          class="mono"
          value={r.value}
          placeholder={placeholders[r.what]}
          spellcheck="false"
          aria-label={t('Значение')}
          onchange={(e) => patch(i, { value: e.currentTarget.value })}
        />
      {/if}
      <select
        value={r.action}
        class="action"
        data-action={r.action}
        aria-label={t('Действие')}
        onchange={(e) => patch(i, { action: e.currentTarget.value as ExclusionRule['action'] })}
      >
        <option value="no_send">{t('Не отправлять')}</option>
        <option value="no_upload">{t('Не загружать сразу')}</option>
      </select>
      <button class="remove" aria-label={t('Удалить правило')} onclick={() => onchange(rules.filter((_, j) => j !== i))}>✕</button>
    </div>
  {:else}
    <p class="hint">{t('Правил нет')}</p>
  {/each}
</div>
<div class="buttons">
  <button onclick={restore}>{t('Вернуть правила по умолчанию')}</button>
  <button onclick={add}>{t('Добавить правило')}</button>
</div>

<style>
  .rules {
    display: flex;
    flex-direction: column;
  }
  .rule {
    display: grid;
    grid-template-columns: auto minmax(0, 9.5rem) minmax(0, 1fr) minmax(0, 9.5rem) auto;
    gap: var(--space-2);
    align-items: center;
    padding: 6px 0;
  }
  .rule + .rule {
    border-top: 1px solid var(--c-border);
  }
  .rule.off > :not(input[type='checkbox']) {
    opacity: 0.55;
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
  .action[data-action='no_send'] {
    color: var(--c-danger);
  }
  .action[data-action='no_upload'] {
    color: var(--c-accent);
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
    margin: 0;
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
    transition: background var(--dur-fast) var(--ease-out);
  }
  button:hover {
    background: var(--c-hover);
  }
  .remove {
    border: 0;
    padding: 2px 8px;
    color: var(--c-muted);
  }
</style>
