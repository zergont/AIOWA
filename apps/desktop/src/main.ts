import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import SetupWizard from "./lib/components/SetupWizard.svelte";
import { api, errorMessage, type AppMode } from "./lib/api";
import { restoreTheme } from "./lib/settings.svelte";

// Тема прошлого запуска — до первой отрисовки: настройки из базы придут позже, окно не должно мигнуть.
restoreTheme();

const target = document.getElementById("app")!;

// Режим выбирает бэкенд при старте процесса. Без учётной записи ("setup") состояния почты нет,
// поэтому почтовое окно (App) даже не монтируется: ни его команд, ни подписок, ни таймеров —
// на весь экран только мастер. Старый бэкенд без app_mode — режим почты.
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
