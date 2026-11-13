<script lang="ts">
  // Карточка ссылки в предпросмотре: заголовок, описание и картинка из og-тегов.
  // Грузит Rust-часть (страница не исполняется), картинка приходит data URL.
  import { api, type LinkCard } from '../lib/api'

  let { url }: { url: string } = $props()

  let card = $state<LinkCard | null>(null)
  const host = $derived.by(() => {
    try {
      return new URL(url).host.replace(/^www\./, '')
    } catch {
      return ''
    }
  })

  $effect(() => {
    const u = url
    card = null
    api.linkPreview(u).then((c) => {
      if (u === url) card = c
    })
  })
</script>

<div class="card" aria-live="polite">
  {#if !card}
    <div class="shot skeleton"></div>
    <p class="site">{host}</p>
    <p class="muted">Загружаю карточку…</p>
  {:else if card.status === 'ok'}
    {#if card.image}
      <img class="shot" src={card.image} alt="" draggable="false" />
    {/if}
    <p class="site">{card.site}</p>
    {#if card.title}<p class="title">{card.title}</p>{/if}
    {#if card.description}<p class="desc">{card.description}</p>{/if}
  {:else}
    <p class="site">{card.site}</p>
    <p class="muted">
      {#if card.status === 'one_time'}
        Похоже на одноразовую ссылку: вход, сброс пароля или подтверждение. Карточку не загружаю,
        чтобы переход не сжёг её.
      {:else if card.status === 'off'}
        Карточки ссылок выключены в настройках, в разделе «Приватность».
      {:else}
        Карточка не загрузилась: {card.error}
      {/if}
    </p>
  {/if}
  <p class="url">{url}</p>
</div>

<style>
  .card {
    flex: 1;
    min-height: 0;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-3);
    border: 1px solid var(--c-border);
    border-radius: var(--radius-m);
    background: var(--c-bg);
  }
  p {
    margin: 0;
  }
  .shot {
    display: block;
    width: 100%;
    aspect-ratio: 1.91;
    object-fit: cover;
    border-radius: var(--radius-s);
    background: var(--c-surface);
    margin-bottom: var(--space-2);
  }
  .skeleton {
    animation: pulse 1.2s ease-in-out infinite alternate;
  }
  @keyframes pulse {
    to {
      opacity: 0.45;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .skeleton {
      animation: none;
    }
  }
  .site {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
  }
  .title {
    font-weight: 600;
    line-height: 1.3;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .desc {
    font-size: var(--text-s);
    color: var(--c-muted);
    line-height: 1.4;
    display: -webkit-box;
    -webkit-line-clamp: 5;
    line-clamp: 5;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .muted {
    font-size: var(--text-s);
    color: var(--c-muted);
    line-height: 1.4;
  }
  .url {
    margin-top: auto;
    padding-top: var(--space-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
    overflow-wrap: anywhere;
  }
</style>
