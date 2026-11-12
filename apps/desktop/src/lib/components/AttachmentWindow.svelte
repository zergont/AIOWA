<script lang="ts">
  /**
   * Окно просмотра вложений письма (п. 31 обкатки): своё окно Windows вместо слоя поверх главного.
   * Внутри — тот же просмотр (`AttachmentPreview` в режиме окна): картинка, PDF, текст или вложенное
   * письмо, «Открыть», «Сохранить как…», стрелки по вложениям письма. Бэкенд поднимает это окно и
   * присылает `attachment-show`, когда в главном окне щёлкают другое вложение того же письма.
   */
  import { onMount } from "svelte";
  import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
  import { api, errorMessage, isInlinePicture, type Attachment } from "../api";
  import AttachmentPreview from "./AttachmentPreview.svelte";

  interface Props {
    itemPk: number;
    attachmentPk: number;
  }
  let { itemPk, attachmentPk }: Props = $props();

  let files = $state<Attachment[]>([]);
  let index = $state(0);
  let loadError = $state<string | null>(null);
  type Status = { busy: "open" | "save" | null; error: string | null; saved: string | null };
  let statuses = $state<Record<number, Status>>({});
  const savedTimers = new Map<number, ReturnType<typeof setTimeout>>();

  const status = (pk: number): Status => statuses[pk] ?? { busy: null, error: null, saved: null };
  function setStatus(pk: number, patch: Partial<Status>) {
    statuses = { ...statuses, [pk]: { ...status(pk), ...patch } };
  }

  /** Вложения письма заново (после события о другом вложении — вдруг список изменился) и вложение `pk` открытым. */
  async function load(pk: number) {
    try {
      const list = (await api.itemAttachments(itemPk)).filter((a) => !isInlinePicture(a));
      files = list;
      const i = list.findIndex((a) => a.attachment_pk === pk);
      index = i >= 0 ? i : 0;
      loadError = list.length ? null : "У письма больше нет вложений";
    } catch (e) {
      loadError = errorMessage(e);
    }
  }

  const win = getCurrentWebviewWindow();
  const close = () => void win.close().catch(() => {});

  onMount(() => {
    void load(attachmentPk);
    const off = win.listen<number>("attachment-show", (e) => {
      const i = files.findIndex((a) => a.attachment_pk === e.payload);
      if (i >= 0) index = i;
      else void load(e.payload);
    });
    return () => {
      void off.then((f) => f());
      for (const t of savedTimers.values()) clearTimeout(t);
    };
  });

  // Заголовок окна на панели задач и в Alt+Tab — имя открытого вложения.
  $effect(() => {
    const name = files[index]?.name;
    if (name) void win.setTitle(name).catch(() => {});
  });

  async function openFile(a: Attachment) {
    const pk = a.attachment_pk;
    if (status(pk).busy) return;
    setStatus(pk, { busy: "open", error: null, saved: null });
    try {
      // Вложенное письмо системе не отдаётся (true): оно и так показано в этом окне.
      await api.attachmentOpen(pk);
    } catch (e) {
      setStatus(pk, { error: errorMessage(e) });
    } finally {
      setStatus(pk, { busy: null });
    }
  }

  async function saveFile(a: Attachment) {
    const pk = a.attachment_pk;
    if (status(pk).busy) return;
    setStatus(pk, { busy: "save", error: null, saved: null });
    try {
      const path = await api.attachmentSaveAs(pk);
      if (path != null) {
        setStatus(pk, { saved: path });
        clearTimeout(savedTimers.get(pk));
        savedTimers.set(
          pk,
          setTimeout(() => setStatus(pk, { saved: null }), 8000),
        );
      }
    } catch (e) {
      setStatus(pk, { error: errorMessage(e) });
    } finally {
      setStatus(pk, { busy: null });
    }
  }
</script>

{#if files.length}
  <AttachmentPreview {files} bind:index onopen={openFile} onsave={saveFile} status={status(files[index]?.attachment_pk ?? -1)} onclearerror={(a) => setStatus(a.attachment_pk, { error: null })} onclose={close} />
{:else}
  <main class="empty">
    <p class="caption">{loadError ?? "Загружаю вложения…"}</p>
    <button class="btn" onclick={close}>Закрыть</button>
  </main>
{/if}

<style>
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-3);
    background: var(--bg-base);
  }
  .btn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
  }
</style>
