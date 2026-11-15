<script lang="ts">
  // Поле правки подписи закрепа — на месте заголовка строки или предпросмотра.
  // Enter сохраняет, Esc и уход фокуса отменяют, пустая подпись убирается.
  // Клавиши дальше поля не уходят: окно вставки иначе приняло бы Enter за
  // вставку, Esc за закрытие, а стрелки — за ход по списку.
  import { onMount } from 'svelte'
  import { t } from './i18n'
  import { NOTE_MAX } from './pins'

  let {
    value,
    onsave,
    oncancel,
  }: {
    value: string
    onsave: (text: string) => void
    oncancel: () => void
  } = $props()

  // Начальное значение берётся один раз: поле правят здесь, а не снаружи.
  // svelte-ignore state_referenced_locally
  let text = $state(value)
  let el: HTMLInputElement | undefined = $state()
  // Enter уже сохранил: уход фокуса после этого не отмена.
  let done = false

  onMount(() => {
    // Без прокрутки: строку в окно вернёт сам список (Popup.svelte, startNote).
    el?.focus({ preventScroll: true })
    el?.select()
  })

  function onkeydown(e: KeyboardEvent) {
    e.stopPropagation()
    if (e.isComposing) return
    if (e.key === 'Enter') {
      e.preventDefault()
      done = true
      onsave(text)
    } else if (e.key === 'Escape') {
      e.preventDefault()
      done = true
      oncancel()
    }
  }
</script>

<input
  bind:this={el}
  bind:value={text}
  class="note-edit"
  maxlength={NOTE_MAX}
  spellcheck="false"
  placeholder={t('Подпись: Enter — сохранить, Esc — отмена')}
  aria-label={t('Подпись закреплённой записи')}
  {onkeydown}
  onblur={() => {
    if (!done) oncancel()
  }}
  onclick={(e) => e.stopPropagation()}
  ondblclick={(e) => e.stopPropagation()}
  oncontextmenu={(e) => e.stopPropagation()}
/>

<style>
  /* Та же высота строки, что у заголовка-подписи: строка списка не прыгает,
   * когда подпись превращается в поле. */
  .note-edit {
    display: block;
    width: 100%;
    min-width: 0;
    margin: 0;
    padding: 0 4px;
    border: 0;
    border-radius: var(--radius-s);
    box-shadow: inset 0 0 0 1px var(--c-accent);
    background: var(--c-bg);
    color: var(--c-text);
    font: inherit;
    font-weight: 500;
    line-height: inherit;
    outline: none;
  }
  .note-edit::placeholder {
    color: var(--c-muted);
    font-weight: 400;
  }
</style>
