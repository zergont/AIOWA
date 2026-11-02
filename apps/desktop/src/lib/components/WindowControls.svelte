<script lang="ts">
  /**
   * Кнопки окна в своей шапке (п. 14 плана, замечание 7): «Свернуть», «Развернуть» / «Восстановить»,
   * «Закрыть» — как у Windows 11: 46 px шириной на всю высоту полосы, глифы Segoe Fluent Icons.
   * «Закрыть» идёт через обычный CloseRequested, поэтому несохранённое письмо держит окно
   * (`window_close_guard`), как и с системным крестиком. Наведение на «Развернуть» открывает
   * раскладки Snap (`window_snap_layouts`): системного наведения у окна без рамки нет.
   */
  import { getCurrentWindow, type Window } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import { api, errorMessage } from "../api";

  /** Задержка до раскладок Snap: столько же ждёт системная кнопка. */
  const SNAP_HOVER_MS = 500;

  let maximized = $state(false);
  let focused = $state(true);
  let snapTimer: ReturnType<typeof setTimeout> | null = null;

  /** Окно Tauri; без него (vite-превью) кнопки ничего не делают. */
  function current(): Window | null {
    try {
      return getCurrentWindow();
    } catch {
      return null;
    }
  }

  onMount(() => {
    const w = current();
    if (!w) return;
    const syncMaximized = () => {
      w.isMaximized()
        .then((m) => (maximized = m))
        .catch(() => {});
    };
    syncMaximized();
    const offs = [w.onResized(syncMaximized), w.onFocusChanged((e) => (focused = e.payload))];
    return () => {
      cancelSnap();
      for (const off of offs) off.then((f) => f()).catch(() => {});
    };
  });

  function armSnap() {
    cancelSnap();
    snapTimer = setTimeout(() => {
      snapTimer = null;
      // Win+Z открывает раскладки окна на переднем плане: из фонового окна — не наше.
      if (!document.hasFocus()) return;
      api.windowSnapLayouts().catch((e) => console.warn("window_snap_layouts:", errorMessage(e)));
    }, SNAP_HOVER_MS);
  }

  function cancelSnap() {
    if (snapTimer) clearTimeout(snapTimer);
    snapTimer = null;
  }

  function run(action: (w: Window) => Promise<void>, name: string) {
    cancelSnap();
    const w = current();
    if (w) action(w).catch((e) => console.warn(`${name}:`, errorMessage(e)));
  }
</script>

<!-- Не в порядке Tab, как и системные кнопки: окно сворачивают и закрывают клавишами Windows. -->
<div class="controls" class:inactive={!focused}>
  <button type="button" class="ctl" tabindex="-1" aria-label="Свернуть" onclick={() => run((w) => w.minimize(), "minimize")}>{""}</button>
  <button type="button" class="ctl" tabindex="-1" aria-label={maximized ? "Восстановить" : "Развернуть"} onpointerenter={armSnap} onpointerleave={cancelSnap} onclick={() => run((w) => w.toggleMaximize(), "toggleMaximize")}>{maximized ? "" : ""}</button>
  <button type="button" class="ctl close" tabindex="-1" aria-label="Закрыть" onclick={() => run((w) => w.close(), "close")}>{""}</button>
</div>

<style>
  .controls {
    display: flex;
    align-self: stretch;
    flex: none;
  }
  .ctl {
    width: 46px;
    height: 100%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-family: "Segoe Fluent Icons", "Segoe MDL2 Assets";
    font-size: 10px;
    color: var(--fg-1);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .ctl:hover {
    background: var(--bg-hover);
  }
  .ctl:active {
    background: var(--bg-pressed);
  }
  .close:hover,
  .close:active {
    background: var(--caption-close-bg);
    color: var(--caption-close-fg);
  }
  /* Окно не в фокусе: глифы тусклые, как у системных кнопок. */
  .inactive .ctl:not(:hover) {
    color: var(--fg-disabled);
  }
</style>
