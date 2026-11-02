import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import SetupWizard from "./lib/components/SetupWizard.svelte";
import AttachmentWindow from "./lib/components/AttachmentWindow.svelte";
import { api, errorMessage, type AppMode } from "./lib/api";
import { restoreTheme } from "./lib/settings.svelte";

// Тема прошлого запуска — до первой отрисовки: настройки из базы придут позже, окно не должно мигнуть.
restoreTheme();

const target = document.getElementById("app")!;

// Окно просмотра вложений (п. 31): та же страница с `?view=attachments` — своё окно, без почты.
const query = new URLSearchParams(location.search);
const attachmentView = query.get("view") === "attachments" ? { itemPk: Number(query.get("item")), attachmentPk: Number(query.get("att")) } : null;
if (attachmentView && Number.isFinite(attachmentView.itemPk) && Number.isFinite(attachmentView.attachmentPk)) {
  mount(AttachmentWindow, { target, props: attachmentView });
} else {
  mountMain();
}

// Режим выбирает бэкенд при старте процесса. Без учётной записи ("setup") состояния почты нет,
// поэтому почтовое окно (App) даже не монтируется: ни его команд, ни подписок, ни таймеров —
// на весь экран только мастер. Старый бэкенд без app_mode — режим почты.
function mountMain() {
  api
    .appMode()
    .catch((e): AppMode | null => {
      console.warn("app_mode:", errorMessage(e));
      return null;
    })
    .then((mode) => {
      if (mode?.mode === "setup") mount(SetupWizard, { target, props: { mode } });
      else mount(App, { target, props: { autoSync: mode?.autostart_sync !== false } });
    });
}
