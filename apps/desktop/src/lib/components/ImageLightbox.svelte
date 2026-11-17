<script lang="ts">
  /**
   * Картинка из письма крупно (п. 41 обкатки): щелчок по картинке в тексте письма открывает её
   * поверх окна — целиком, по размеру окна. Если картинка больше окна, щелчок по ней показывает
   * настоящий размер с прокруткой, ещё щелчок — возвращает. Esc, крестик или щелчок мимо закрывают.
   */
  import { isTopLayer, layer } from "../hotkeys";

  interface Props {
    /** Адрес картинки: `data:` из тела письма. */
    src: string;
    alt?: string;
    onclose: () => void;
  }
  let { src, alt = "", onclose }: Props = $props();

  let root = $state<HTMLDivElement | null>(null);
  let closeBtn = $state<HTMLButtonElement | null>(null);
  let img = $state<HTMLImageElement | null>(null);
  /** Настоящий размер с прокруткой вместо «целиком в окне». */
  let full = $state(false);
  /** Картинка в окно целиком не помещается: есть что увеличивать. */
  let larger = $state(false);

  // Картинку в письме нередко открывают двойным щелчком: второй щелчок попал бы уже по слою и
  // сразу переключил бы размер или закрыл его. Первые доли секунды щелчки мыши слой не слушает.
  const OPEN_GUARD_MS = 400;
  const openedAt = performance.now();
  const settled = () => performance.now() - openedAt >= OPEN_GUARD_MS;

  function measure() {
    const i = img;
    if (!i || full) return;
    larger = i.naturalWidth > i.clientWidth + 1 || i.naturalHeight > i.clientHeight + 1;
  }

  /** Слой живёт в body: его не обрежут ни прокрутка письма, ни рамки карточек. */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return () => node.remove();
  }

  $effect(() => {
    closeBtn?.focus();
  });

  function onKey(e: KeyboardEvent) {
    if (!isTopLayer(root)) return;
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
    } else if (e.key === "Tab") {
      // В слое две кнопки: фокус ходит между ними и не уходит под слой.
      const zoom = root?.querySelector<HTMLButtonElement>(".pic:not(:disabled)");
      e.preventDefault();
      (document.activeElement === closeBtn && zoom ? zoom : closeBtn)?.focus();
    }
  }
</script>

<svelte:window onkeydown={onKey} onresize={measure} />

<div class="veil" role="dialog" aria-modal="true" aria-label="Картинка из письма" bind:this={root} {@attach portal} {@attach layer}>
  <button class="backdrop" tabindex="-1" aria-label="Закрыть" onclick={() => settled() && onclose()}></button>
  <div class="stage" class:full>
    <button class="pic" class:zoomable={larger} disabled={!larger} onclick={() => settled() && (full = !full)} aria-label={full ? "Показать целиком" : "Показать в настоящем размере"} title={larger ? (full ? "Показать целиком" : "Показать в настоящем размере") : undefined}>
      <img bind:this={img} {src} {alt} onload={measure} />
    </button>
  </div>
  <button class="close" bind:this={closeBtn} onclick={onclose} aria-label="Закрыть" title="Закрыть (Esc)">
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
  </button>
</div>

<style>
  .veil {
    position: fixed;
    inset: 0;
    z-index: 200;
    background: rgba(0, 0, 0, 0.82);
  }
  .backdrop {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    cursor: default;
  }
  /* Целиком: картинка по центру, щелчок мимо неё проходит к подложке и закрывает слой. */
  .stage {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 40px;
    pointer-events: none;
  }
  /* Настоящий размер: область с прокруткой на всё окно. */
  .stage.full {
    display: block;
    padding: 0;
    overflow: auto;
    pointer-events: auto;
  }
  .pic {
    display: block;
    padding: 0;
    pointer-events: auto;
    cursor: default;
    line-height: 0;
  }
  .pic.zoomable {
    cursor: zoom-in;
  }
  .stage.full .pic {
    cursor: zoom-out;
    margin: 0 auto;
  }
  /* Письма рассчитаны на белый лист: у картинки с прозрачным фоном он остаётся белым. */
  img {
    display: block;
    max-width: calc(100vw - 80px);
    max-height: calc(100vh - 80px);
    background: #fff;
    border-radius: 2px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
  }
  .stage.full img {
    max-width: none;
    max-height: none;
    border-radius: 0;
    box-shadow: none;
  }
  .close {
    position: absolute;
    top: 12px;
    right: 16px;
    width: 36px;
    height: 36px;
    display: flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    background: rgba(32, 32, 32, 0.85);
    color: #fff;
    border: 1px solid rgba(255, 255, 255, 0.25);
  }
  .close:hover {
    background: rgba(64, 64, 64, 0.95);
  }
  .close:focus-visible,
  .pic:focus-visible {
    outline: 2px solid #fff;
    outline-offset: 2px;
  }
</style>
