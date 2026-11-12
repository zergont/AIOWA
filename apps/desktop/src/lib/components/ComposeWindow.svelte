<script lang="ts">
  /**
   * Окно черновика (п. 32 обкатки): письмо в работе — своё окно Windows. Открывается кнопкой
   * «Редактировать в отдельном окне» в композере (письмо приходит локальным черновиком `localId`) и
   * двойным щелчком по черновику в «Черновиках» (`draftPk`). Есть локальный черновик — окно поднимает
   * его (там последние правки), иначе открывает черновик с сервера. Несохранённое письмо держит окно:
   * крестик спрашивает, как и «Отменить»; главное окно, закрываясь, сначала спрашивает это окно.
   */
  import { onMount } from "svelte";
  import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
  import { api, composeFinished, contextFromLocal, errorMessage, onCloseRequested, type ComposeContext, type LocalDraft } from "../api";
  import { COMPOSE_TITLE } from "../actions";
  import { preventBrowserKeys } from "../hotkeys";
  import Composer, { type CloseReason } from "./Composer.svelte";
  import WindowTitleBar from "./WindowTitleBar.svelte";

  interface Props {
    /** Локальный черновик окна (его же ключ — в имени окна у бэкенда). */
    localId: string;
    /** Черновик из «Черновиков», если окно открыто им. */
    draftPk: number | null;
  }
  let { localId, draftPk }: Props = $props();

  const win = getCurrentWebviewWindow();

  let ctx = $state<ComposeContext | null>(null);
  let loadError = $state<string | null>(null);
  let dirty = $state(false);
  let busy = $state<"send" | "draft" | null>(null);
  let heading = $state("");
  let composerRef = $state<ReturnType<typeof Composer> | null>(null);
  let innerHeight = $state(760);
  /** Композер закрылся, окно закрывается: держать его больше нечем. */
  let closing = $state(false);

  const title = $derived(heading || (ctx ? (ctx.draft ? "Черновик" : COMPOSE_TITLE[ctx.kind]) : "Письмо"));
  $effect(() => {
    const t = title;
    void win.setTitle(t).catch(() => {});
  });

  /** Письмо, перенесённое из композера, — с вложениями, уже лежащими на его черновике на сервере. */
  async function fromLocal(d: LocalDraft): Promise<ComposeContext> {
    const c = await contextFromLocal(d);
    if (!d.draft_server_id) return c;
    try {
      return { ...c, restore_files: await api.composeDraftAttachments(d.draft_server_id) };
    } catch (e) {
      // Нет связи: файлы черновика окно узнает после сохранения, как раньше.
      console.warn("compose_draft_attachments:", errorMessage(e));
      return c;
    }
  }

  async function load() {
    try {
      const local = await api.composeLocalGet(localId);
      if (local) ctx = await fromLocal(local);
      else if (draftPk != null) ctx = await api.composeOpenDraft(draftPk);
      else loadError = "Письма уже нет: его отправили или закрыли";
    } catch (e) {
      loadError = errorMessage(e);
    }
  }

  // Несохранённое письмо или идущая отправка: крестик и Alt+F4 окно не закрывают, а спрашивают.
  const guarded = $derived(!closing && ctx !== null && (dirty || busy !== null));
  $effect(() => {
    const g = guarded;
    api.windowCloseGuard(g).catch((e) => console.warn("window_close_guard:", errorMessage(e)));
  });

  async function onCloseRequest() {
    if (!guarded || !composerRef) {
      void closeWindow();
      return;
    }
    // Ответ придёт в onclose (композер закроется) или null — пользователь вернулся к письму.
    await composerRef.askClose();
  }

  /** Композер закрылся: итог — главному окну (синхронизация папок, строка уведомления), окно закрывается. */
  async function onclose(reason: CloseReason) {
    if (closing) return;
    closing = true;
    // Записи локального черновика, начатые композером, должны дойти до базы раньше, чем окно
    // закроется: порядок держит очередь композера.
    await composerRef?.localSettled();
    // Локальный черновик убирается до закрытия: главное окно перечитает их сразу после.
    if (reason !== "local" && reason !== "window") await api.composeLocalDrop(localId).catch(() => {});
    if (reason !== "cancel") await composeFinished(reason).catch((e) => console.warn("compose-finished:", errorMessage(e)));
    await closeWindow();
  }

  async function closeWindow() {
    try {
      await api.windowCloseConfirmed();
    } catch {
      await win.close().catch(() => {});
    }
  }

  onMount(() => {
    void load();
    const off = onCloseRequested(() => void onCloseRequest());
    return () => void off.then((f) => f());
  });
</script>

<svelte:window bind:innerHeight onkeydowncapture={preventBrowserKeys} />

<div class="window">
  <WindowTitleBar {title} />
  <main class="body">
    {#if ctx}
      <Composer bind:this={composerRef} {ctx} bind:dirty bind:busy bind:heading standalone localDraftId={localId} editorMinHeight={Math.max(240, innerHeight - 320)} {onclose} />
    {:else}
      <div class="empty">
        <p class="caption">{loadError ?? "Открываю письмо…"}</p>
        {#if loadError}<button class="btn" onclick={() => void closeWindow()}>Закрыть</button>{/if}
      </div>
    {/if}
  </main>
</div>

<style>
  .window {
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-3);
    padding: var(--space-6);
    text-align: center;
  }
  .btn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
  }
</style>
