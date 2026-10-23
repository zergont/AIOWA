/** Всплывающие окна (меню, выбор папки, подтверждение): положение у якоря и закрытие по действиям снаружи. */

/** Якорь всплывающего окна: элемент (кнопка, строка) или точка (курсор при правом клике). */
export type Anchor = HTMLElement | { x: number; y: number };

export interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

/**
 * Вернуть фокус элементу после закрытия окна — кроме случая, когда фокус ушёл во фрейм письма:
 * пользователь кликнул по тексту письма, и отбирать фокус у фрейма нельзя.
 */
export function focusBack(el: HTMLElement | null | undefined) {
  if (!el || document.activeElement instanceof HTMLIFrameElement) return;
  el.focus();
}

/** Якорь-кнопка (для `dismissOn`), если якорь — элемент. */
export function anchorToggle(a: Anchor): HTMLElement | undefined {
  return a instanceof HTMLElement ? a : undefined;
}

export function anchorRect(a: Anchor): Rect {
  if (a instanceof HTMLElement) {
    const r = a.getBoundingClientRect();
    return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  }
  return { left: a.x, top: a.y, right: a.x, bottom: a.y };
}

const MARGIN = 8;
const GAP = 4;

/**
 * Положение окна размером w×h под якорем (или над ним, если снизу не помещается), в пределах
 * видимой области с отступом. `align: "end"` прижимает правый край окна к правому краю якоря.
 */
export function placePopup(a: Rect, w: number, h: number, align: "start" | "end" = "start"): { left: number; top: number } {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  let left = align === "end" ? a.right - w : a.left;
  let top = a.bottom + GAP;
  if (top + h > vh - MARGIN) {
    const above = a.top - GAP - h;
    top = above >= MARGIN ? above : Math.max(MARGIN, vh - MARGIN - h);
  }
  left = Math.min(Math.max(MARGIN, left), Math.max(MARGIN, vw - MARGIN - w));
  return { left, top };
}

/**
 * Закрытие окна по действиям снаружи: нажатие указателя вне окна, уход фокуса из окна клавиатурой,
 * потеря фокуса самим окном приложения (клик по фрейму письма не доходит до документа — он уводит фокус).
 * `toggle` — кнопка, открывшая окно: нажатие на неё не закрывает окно здесь, иначе её собственный клик
 * тут же открыл бы окно заново; закрыть его — дело обработчика кнопки.
 * Возвращает функцию отписки.
 */
export function dismissOn(el: HTMLElement, close: () => void, toggle?: HTMLElement): () => void {
  const inside = (n: EventTarget | null) => n instanceof Node && (el.contains(n) || (toggle?.contains(n) ?? false));
  const onPointer = (e: PointerEvent) => {
    if (!inside(e.target)) close();
  };
  const onFocusOut = (e: FocusEvent) => {
    const to = e.relatedTarget;
    if (to instanceof Node && !inside(to)) close();
  };
  const onBlur = () => close();
  document.addEventListener("pointerdown", onPointer, true);
  el.addEventListener("focusout", onFocusOut);
  window.addEventListener("blur", onBlur);
  return () => {
    document.removeEventListener("pointerdown", onPointer, true);
    el.removeEventListener("focusout", onFocusOut);
    window.removeEventListener("blur", onBlur);
  };
}
