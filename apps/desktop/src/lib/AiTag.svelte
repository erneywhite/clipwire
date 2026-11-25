<script lang="ts">
  // Метка «ИИ · клиент» у записи, которую добавил ИИ-инструмент через MCP.
  // Стоит в строке меты рядом с AppTag: в окне вставки, в предпросмотре и в
  // примере окна в настройках. Цвет — фирменный цвет клиента (aiBrand), у
  // незнакомых — фиолетовый --c-ai темы. Подпись подмешивает цвет текста темы,
  // чтобы читаться и на тёмном, и на светлом фоне.
  import { t } from './i18n'
  import { aiBrand } from './aiBrand'

  let { client }: { client: string } = $props()
  const brand = $derived(aiBrand(client))
</script>

<span
  class="ai"
  class:brand={!!brand}
  style:--brand={brand}
  title={t('Добавлено ИИ-инструментом «{client}»', { client })}
  >{t('ИИ')}<span class="sep">·</span><span class="name">{client}</span></span
>

<style>
  .ai {
    display: inline-flex;
    align-items: baseline;
    gap: 3px;
    max-width: 22ch;
    padding: 0 4px;
    border-radius: 4px;
    background: var(--c-ai-soft);
    color: var(--c-ai);
    vertical-align: bottom;
  }
  .ai.brand {
    background: color-mix(in oklch, var(--brand) 18%, transparent);
    color: color-mix(in oklch, var(--brand) 62%, var(--c-text));
  }
  .sep {
    opacity: 0.7;
  }
  .name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
