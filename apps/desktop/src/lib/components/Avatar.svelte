<script lang="ts">
  import { initials } from "../contact";
  import { photoUrl } from "../photos.svelte";

  /** Кружок человека: фото из кэша окна (или переданное), иначе инициалы. */
  interface Props {
    name: string | null | undefined;
    email: string | null | undefined;
    /** Диаметр, px. */
    size?: number;
    /** Фото, пришедшее вместе с данными (карточка): важнее кэша. */
    src?: string | null;
  }
  let { name, email, size = 36, src }: Props = $props();
  const url = $derived(src ?? photoUrl(email) ?? null);
  const fontSize = $derived(Math.max(10, Math.round(size * 0.38)));
</script>

<span class="avatar" class:photo={!!url} style="--size: {size}px; --font-size: {fontSize}px" aria-hidden="true">
  {#if url}
    <img src={url} alt="" draggable="false" />
  {:else}
    {initials(name, email)}
  {/if}
</span>

<style>
  .avatar {
    flex: none;
    width: var(--size);
    height: var(--size);
    border-radius: 50%;
    background: var(--accent-soft);
    color: var(--accent);
    font-family: var(--font-ui);
    font-size: var(--font-size);
    font-weight: 600;
    line-height: 1;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
    user-select: none;
  }
  .avatar.photo {
    background: var(--bg-layer);
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
</style>
