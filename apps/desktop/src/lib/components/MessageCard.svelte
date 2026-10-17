<script lang="ts">
  import { formatSize, fullDate, listDate, personLabel, type Message } from "../api";

  interface Props {
    message: Message;
    expanded: boolean;
    ontoggle: () => void;
  }
  let { message, expanded, ontoggle }: Props = $props();

  const to = $derived(message.recipients.filter((r) => r.kind === 1).map((r) => personLabel(r.name, r.email)));
  const cc = $derived(message.recipients.filter((r) => r.kind === 2).map((r) => personLabel(r.name, r.email)));
  const files = $derived(message.attachments.filter((a) => !a.is_inline));

  // Письмо живёт в изолированном фрейме: без скриптов, без внешних ресурсов, своя «бумага».
  const srcdoc = $derived.by(() => {
    const body = message.body_html ?? "";
    return `<!doctype html><html lang="ru"><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data: cid:; style-src 'unsafe-inline'; font-src 'none'; frame-src 'none'">
<style>
  html,body{margin:0;background:#fff;color:#1f1f1f}
  body{font:14px/1.5 "Segoe UI Variable Text","Segoe UI",Inter,system-ui,sans-serif;padding:16px 20px;word-wrap:break-word;overflow-wrap:anywhere}
  img{max-width:100%;height:auto}
  table{max-width:100%}
  pre{white-space:pre-wrap}
  blockquote{border-left:3px solid #ddd;margin:8px 0;padding:0 12px;color:#555}
  a{color:#005fb8}
</style></head><body>${body}</body></html>`;
  });

  let frame = $state<HTMLIFrameElement | null>(null);
  let height = $state(120);
  function fit() {
    const doc = frame?.contentDocument;
    if (!doc) return;
    height = Math.min(20000, Math.max(80, doc.documentElement.scrollHeight + 2));
  }
  $effect(() => {
    void srcdoc;
    // Высота пересчитывается после загрузки и при подгрузке картинок.
    const id = setTimeout(fit, 50);
    return () => clearTimeout(id);
  });

  function initials(name: string | null, email: string | null): string {
    const s = (name || email || "?").trim();
    const parts = s.split(/\s+/).filter(Boolean);
    return parts.length >= 2 ? (parts[0][0] + parts[1][0]).toUpperCase() : s.slice(0, 2).toUpperCase();
  }
</script>

<article class="card" class:expanded>
  <header class="head" onclick={ontoggle} role="button" tabindex="0" onkeydown={(e) => (e.key === "Enter" || e.key === " ") && ontoggle()} aria-expanded={expanded}>
    <div class="avatar" aria-hidden="true">{initials(message.from_name, message.from_email)}</div>
    <div class="who">
      <div class="from">
        <span class="name">{personLabel(message.from_name, message.from_email) || "(без отправителя)"}</span>
        {#if message.from_name && message.from_email}<span class="email caption">{message.from_email}</span>{/if}
        {#if message.importance === "High"}<span class="badge high">важное</span>{/if}
        {#if message.is_draft}<span class="badge">черновик</span>{/if}
      </div>
      {#if expanded}
        <div class="line caption">Кому: {to.length ? to.join(", ") : message.display_to || "—"}</div>
        {#if cc.length}<div class="line caption">Копия: {cc.join(", ")}</div>{/if}
      {:else}
        <div class="line caption preview">{message.preview || ""}</div>
      {/if}
    </div>
    <div class="when">
      <span class="date" title={fullDate(message.received_ms ?? message.sent_ms)}>{expanded ? fullDate(message.received_ms ?? message.sent_ms) : listDate(message.received_ms ?? message.sent_ms)}</span>
      {#if message.folder_name}<span class="caption folder">{message.folder_name}</span>{/if}
    </div>
  </header>

  {#if expanded}
    {#if files.length}
      <div class="files">
        {#each files as a (a.attachment_pk)}
          <div class="file" class:cached={a.cached} title={a.content_type ?? ""}>
            <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 2.5h7l3 3v8H3v-11Zm7 0v3h3" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
            <span class="file-name">{a.name ?? "вложение"}</span>
            <span class="caption">{formatSize(a.size)}</span>
          </div>
        {/each}
      </div>
    {/if}
    <div class="body">
      {#if message.body_html != null}
        <iframe bind:this={frame} title="Текст письма" sandbox="allow-same-origin" {srcdoc} style="height: {height}px" onload={fit}></iframe>
        {#if message.is_truncated}<div class="caption note">Письмо обрезано при загрузке.</div>{/if}
      {:else if message.body_state === 2}
        <div class="caption note">Тело письма не удалось загрузить с сервера.</div>
      {:else}
        <div class="caption note">Тело письма ещё не загружено. Оно появится после следующей синхронизации с нужной глубиной.</div>
        {#if message.preview}<p class="preview-text">{message.preview}</p>{/if}
      {/if}
    </div>
  {/if}
</article>

<style>
  .card {
    /* Карточки лежат в прокручиваемой flex-колонке: без flex: none они сжимаются в полоски. */
    flex: none;
    background: var(--bg-card);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-2);
    overflow: hidden;
  }
  .head {
    display: grid;
    grid-template-columns: 36px 1fr auto;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    align-items: start;
    cursor: default;
  }
  .card:not(.expanded) .head:hover {
    background: var(--bg-hover);
  }
  .avatar {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    background: var(--accent-soft);
    color: var(--accent);
    font: var(--text-body-strong);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .who {
    min-width: 0;
  }
  .from {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
    flex-wrap: wrap;
  }
  .name {
    font: var(--text-body-strong);
    user-select: text;
  }
  .email {
    user-select: text;
  }
  .badge {
    font: var(--text-caption);
    border: 1px solid var(--stroke-2);
    border-radius: 9px;
    padding: 0 6px;
    line-height: 16px;
    color: var(--fg-3);
  }
  .badge.high {
    color: var(--danger);
    border-color: var(--danger);
  }
  .line {
    margin-top: 2px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .when {
    text-align: right;
    display: flex;
    flex-direction: column;
    gap: 2px;
    white-space: nowrap;
  }
  .date {
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .files {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    padding: 0 var(--space-4) var(--space-3);
  }
  .file {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    color: var(--fg-2);
    max-width: 320px;
  }
  .file.cached {
    color: var(--fg-1);
  }
  .file-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .body {
    border-top: 1px solid var(--stroke-1);
  }
  iframe {
    display: block;
    width: 100%;
    border: 0;
    background: #fff;
  }
  .note {
    padding: var(--space-3) var(--space-4);
  }
  .preview-text {
    margin: 0;
    padding: 0 var(--space-4) var(--space-4);
    color: var(--fg-2);
    user-select: text;
  }
</style>
