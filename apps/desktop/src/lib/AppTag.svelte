<script lang="ts">
  // Приложение, из которого скопировано: маленькая иконка и имя. Для строки
  // меты в окне вставки и в предпросмотре.
  import type { SourceApp } from './api'

  let { app }: { app: SourceApp } = $props()
</script>

<span class="app" title="Скопировано в «{app.name}»">
  {#if app.icon}<img src={app.icon} alt="" draggable="false" />{/if}<span class="name">{app.name}</span>
</span>

<style>
  .app {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    vertical-align: bottom;
    max-width: 18ch;
  }
  .name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  img {
    width: 14px;
    height: 14px;
    flex: none;
  }
</style>
