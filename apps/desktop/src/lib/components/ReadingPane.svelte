<script lang="ts">
  import { untrack, type Snippet } from "svelte";
  import { conversationsText, type ComposeKind, type Conversation, type Folder, type Message } from "../api";
  import type { ItemAction } from "../actions";
  import MessageCard from "./MessageCard.svelte";

  interface Props {
    conversation: Conversation | null;
    messages: Message[];
    loading: boolean;
    /** Догрузить письмо с сервера; ошибку пробрасывает, карточка покажет её у себя. */
    onfetch: (itemPk: number) => Promise<void>;
    /** Папки для «Переместить…» на карточке. */
    folders: Folder[];
    /** Папка «Удалённые»: письма в ней удаляются только навсегда. */
    deletedFolderPk: number | null;
    /** Папка архива для кнопки «В архив» на карточке; null — папки нет. */
    archiveFolderPk?: number | null;
    /** Папка «Черновики»: у письма в ней вместо панели действий «Открыть черновик». */
    draftsFolderPk?: number | null;
    /** Выбрано несколько бесед в списке: вместо письма — заглушка «Выбрано N бесед». */
    multiCount?: number;
    /** Действие над одним письмом; `anchor` — нажатая кнопка; ошибку пробрасывает, карточка покажет её у себя. */
    onaction: (itemPk: number, action: ItemAction, anchor?: HTMLElement) => Promise<void>;
    /** Ответ, ответ всем или пересылка письма с карточки. */
    oncompose: (itemPk: number, kind: ComposeKind) => void;
    /** Открыть черновик с карточки в композере. */
    onopendraft?: (itemPk: number) => void;
    /** Открыть беседу письма в «Отправленных» (плашка «Вы ответили…» в карточке). */
    onfindsent?: (itemPk: number) => void;
    /** Черновик открывается (может читаться с сервера): кнопка карточки показывает это и не жмётся повторно. */
    draftOpening?: boolean;
    /** Композер: над карточками беседы, а при `composerAlone` — вместо неё (новое письмо, чужая беседа). */
    composer?: Snippet;
    composerAlone?: boolean;
    /** Самое верхнее развёрнутое письмо (для клавиш ответа/пересылки); null, если беседа не показана. */
    topExpandedPk?: number | null;
    /** С карточки дан ответ на приглашение (или отмена убрана из календаря): беседу стоит перечитать. */
    onmeeting?: () => void;
  }
  let { conversation, messages, loading, onfetch, folders, deletedFolderPk, archiveFolderPk = null, draftsFolderPk = null, multiCount = 0, onaction, oncompose, onopendraft, draftOpening = false, composer, composerAlone = false, topExpandedPk = $bindable(null), onmeeting, onfindsent }: Props = $props();

  // Письма идут от новых к старым; самое свежее развёрнуто, остальные свёрнуты до заголовка.
  // Раскрытие пересчитывается только при смене беседы: точечная замена письма (догрузка тела)
  // и перечитывание после синхронизации не должны сворачивать то, что открыл пользователь.
  let expanded = $state<Set<number>>(new Set());
  let expandedFor: number | null = null;
  let knownPks = new Set<number>();
  $effect(() => {
    const pk = conversation?.conversation_pk ?? null;
    const newest = messages[0];
    if (pk === null) {
      expandedFor = null;
      knownPks = new Set();
      return;
    }
    if (expandedFor !== pk) {
      // Новая беседа: письма приходят чуть позже выбора, ждём первую непустую выдачу.
      if (!newest) return;
      expandedFor = pk;
      knownPks = new Set(messages.map((m) => m.item_pk));
      expanded = new Set([newest.item_pk]);
      return;
    }
    // Та же беседа: пришедшее по синхронизации новое письмо сверху раскрываем, остальное не трогаем.
    const fresh = messages.filter((m) => !knownPks.has(m.item_pk));
    if (!fresh.length) return;
    knownPks = new Set(messages.map((m) => m.item_pk));
    if (newest && fresh.some((m) => m.item_pk === newest.item_pk)) {
      untrack(() => {
        if (!expanded.has(newest.item_pk)) expanded = new Set([...expanded, newest.item_pk]);
      });
    }
  });
  const allExpanded = $derived(messages.length > 0 && messages.every((m) => expanded.has(m.item_pk)));
  $effect(() => {
    topExpandedPk = composerAlone ? null : (messages.find((m) => expanded.has(m.item_pk))?.item_pk ?? null);
  });
  function toggle(pk: number) {
    const next = new Set(expanded);
    if (next.has(pk)) next.delete(pk);
    else next.add(pk);
    expanded = next;
  }
  function toggleAll() {
    expanded = allExpanded ? new Set() : new Set(messages.map((m) => m.item_pk));
  }
</script>

<!--
  Композер рендерится ровно в одном месте — первым в .messages: смена беседы или папки под открытым
  ответом меняет только карточки под ним, а не ветку {#if}, иначе экземпляр композера пересоздавался
  бы и набранный текст, получатели и вложения сбрасывались бы к заготовке.
-->
{#if !composer && multiCount > 1}
  <div class="placeholder" aria-live="polite">
    <svg width="48" height="48" viewBox="0 0 48 48" aria-hidden="true"><rect x="10" y="8" width="30" height="22" rx="3" fill="none" stroke="currentColor" stroke-width="1.5" /><rect x="6" y="16" width="30" height="22" rx="3" fill="var(--bg-layer)" stroke="currentColor" stroke-width="1.5" /><path d="m6 19 15 10 15-10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
    <p>Выбрано {conversationsText(multiCount)}</p>
    <p class="caption">Действия — в заголовке списка или в контекстном меню. Esc оставляет одну беседу.</p>
  </div>
{:else if !composer && !conversation}
  <div class="placeholder">
    <svg width="48" height="48" viewBox="0 0 48 48" aria-hidden="true"><rect x="6" y="12" width="36" height="26" rx="3" fill="none" stroke="currentColor" stroke-width="1.5" /><path d="m6 15 18 12 18-12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
    <p>Выбери беседу, чтобы прочитать её здесь.</p>
    <p class="caption">Стрелки или J и K листают список, Ctrl+K открывает поиск.</p>
  </div>
{:else}
  <div class="pane">
    {#if conversation && !composerAlone}
      <header class="head">
        <h1 class="topic">{conversation.topic || messages[0]?.subject || "(без темы)"}</h1>
        <div class="meta caption">
          {#if messages.length > 1}
            {messages.length} писем в беседе
            <button class="link" onclick={toggleAll}>{allExpanded ? "свернуть все" : "развернуть все"}</button>
          {:else if messages.length === 1}
            одно письмо
          {/if}
        </div>
      </header>
    {/if}
    <div class="messages">
      {#if composer}
        {@render composer()}
      {/if}
      {#if !composerAlone}
        {#if loading && messages.length === 0}
          <div class="caption loading">Загрузка…</div>
        {/if}
        {#each messages as m (m.item_pk)}
          <MessageCard message={m} expanded={expanded.has(m.item_pk)} ontoggle={() => toggle(m.item_pk)} {onfetch} {folders} {deletedFolderPk} {archiveFolderPk} {draftsFolderPk} onaction={(a, el) => onaction(m.item_pk, a, el)} oncompose={(kind) => oncompose(m.item_pk, kind)} onopendraft={onopendraft ? () => onopendraft(m.item_pk) : undefined} {draftOpening} {onmeeting} onfindsent={onfindsent ? () => onfindsent(m.item_pk) : undefined} />
        {/each}
      {/if}
    </div>
  </div>
{/if}

<style>
  .placeholder {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: var(--fg-3);
    gap: var(--space-2);
    padding: var(--space-6);
    text-align: center;
  }
  .placeholder p {
    margin: 0;
  }
  .pane {
    height: 100%;
    display: flex;
    flex-direction: column;
  }
  .head {
    padding: var(--space-4) var(--space-6) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
  }
  .topic {
    margin: 0;
    font: var(--text-subtitle);
    user-select: text;
  }
  .meta {
    margin-top: 4px;
    display: flex;
    gap: var(--space-3);
  }
  .link {
    color: var(--accent);
  }
  .link:hover {
    text-decoration: underline;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-3) var(--space-6) var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .loading {
    padding: var(--space-4);
  }
</style>
