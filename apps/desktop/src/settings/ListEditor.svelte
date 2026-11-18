<script lang="ts">
  // Список строк с удалением и полем добавления: программы-исключения,
  // расширения файлов. `parse` превращает введённое в элементы (например,
  // «.MP4, mkv» — в «mp4» и «mkv»); повторы без учёта регистра не добавляются.
  import { t } from '../lib/i18n'

  let {
    items,
    onchange,
    placeholder,
    label,
    parse = (raw: string) => [raw.trim()],
  }: {
    items: string[]
    onchange: (next: string[]) => void
    placeholder: string
    label: string
    parse?: (raw: string) => string[]
  } = $props()

  let draft = $state('')

  function add() {
    const fresh = parse(draft).filter(
      (x, i, all) => x && all.indexOf(x) === i && !items.some((it) => it.toLowerCase() === x.toLowerCase()),
    )
    if (fresh.length) onchange([...items, ...fresh])
    draft = ''
  }
</script>

<ul class="list">
  {#each items as item (item)}
    <li class="chip">
      <span class="mono">{item}</span>
      <button aria-label={t('Убрать {app}', { app: item })} onclick={() => onchange(items.filter((x) => x !== item))}>✕</button>
    </li>
  {:else}
    <li class="hint">{t('Список пуст')}</li>
  {/each}
</ul>
<form
  class="add"
  onsubmit={(e) => {
    e.preventDefault()
    add()
  }}
>
  <input bind:value={draft} {placeholder} spellcheck="false" aria-label={label} />
  <button type="submit" disabled={!draft.trim()}>{t('Добавить')}</button>
</form>

<style>
  .list {
    list-style: none;
    margin: 0 0 var(--space-3);
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    padding: 3px 4px 3px 10px;
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    background: var(--c-bg);
  }
  .chip button {
    border: 0;
    padding: 0 6px;
    color: var(--c-muted);
    background: transparent;
    cursor: pointer;
  }
  .add {
    display: flex;
    gap: var(--space-2);
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-s);
  }
  input {
    width: 100%;
    background: var(--c-bg);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    padding: 8px var(--space-3);
    color: var(--c-text);
  }
  .add button {
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
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
