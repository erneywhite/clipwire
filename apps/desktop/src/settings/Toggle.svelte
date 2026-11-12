<script lang="ts">
  // Переключатель: настоящий checkbox (доступность, клавиатура) с видом тумблера.
  let {
    checked = $bindable(false),
    label,
    hint = '',
    disabled = false,
    onchange,
  }: {
    checked?: boolean
    label: string
    hint?: string
    disabled?: boolean
    onchange?: (v: boolean) => void
  } = $props()
</script>

<label class="row" class:disabled>
  <span class="text">
    <span>{label}</span>
    {#if hint}<span class="hint">{hint}</span>{/if}
  </span>
  <input
    type="checkbox"
    role="switch"
    bind:checked
    {disabled}
    onchange={() => onchange?.(checked)}
  />
</label>

<style>
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    padding: 11px 0;
    cursor: pointer;
  }
  .row.disabled {
    cursor: default;
    opacity: 0.6;
  }
  .text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .hint {
    font-size: var(--text-s);
    color: var(--c-muted);
  }
  input {
    appearance: none;
    flex: none;
    width: 34px;
    height: 20px;
    border-radius: 10px;
    background: var(--c-border);
    position: relative;
    cursor: inherit;
    transition: background var(--dur-fast) var(--ease-out);
    margin: 0;
  }
  input::after {
    content: '';
    position: absolute;
    top: 3px;
    left: 3px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--c-muted);
    transition: transform var(--dur-fast) var(--ease-out), background var(--dur-fast) var(--ease-out);
  }
  input:checked {
    background: var(--c-accent);
  }
  input:checked::after {
    transform: translateX(14px);
    background: var(--c-on-accent);
  }
</style>
