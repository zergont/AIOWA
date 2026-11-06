import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import SetupWizard from "./lib/components/SetupWizard.svelte";
import AttachmentWindow from "./lib/components/AttachmentWindow.svelte";
import MessageWindow from "./lib/components/MessageWindow.svelte";
import ComposeWindow from "./lib/components/ComposeWindow.svelte";
import { api, errorMessage, type AppMode } from "./lib/api";
import { restoreTheme } from "./lib/settings.svelte";

// Тема прошлого запуска — до первой отрисовки: настройки из базы придут позже, окно не должно мигнуть.
restoreTheme();

const target = document.getElementById("app")!;

// Отдельные окна — та же страница с `?view=…` (src-tauri/src/child_windows.rs), без почты главного
// окна: просмотр вложений (п. 31), беседа и письмо в работе (п. 32).
const query = new URLSearchParams(location.search);
const view = query.get("view");
const num = (name: string): number | null => {
  const v = query.get(name);
  const n = v == null || v === "" ? NaN : Number(v);
  return Number.isSafeInteger(n) ? n : null;
};
const itemPk = num("item");
const attachmentPk = num("att");
const conversationPk = num("conv");
const localId = query.get("local");
if (view === "attachments" && itemPk != null && attachmentPk != null) {
  mount(AttachmentWindow, { target, props: { itemPk, attachmentPk } });
} else if (view === "message" && conversationPk != null) {
  mount(MessageWindow, { target, props: { conversationPk } });
} else if (view === "compose" && localId) {
  mount(ComposeWindow, { target, props: { localId, draftPk: num("draft") } });
} else if (view) {
  // Отдельное окно с неверным адресом: почту главного окна (синхронизацию, таймеры) в нём не поднимать.
  target.textContent = "Окно не открылось: неверный адрес";
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
