/**
 * Перетаскивание адресата между полями одного письма — «Кому», «Копия», «Скрытая» (п. 34 обкатки).
 * Чип тянется указателем, а не средствами HTML drag-and-drop: так одинаково работает в главном окне
 * и в отдельных и не зависит от того, принимает ли окно файлы. Поля одного письма объединяет
 * `group`; каждое поле регистрирует свою рамку зоной, чтобы поле-источник нашло поле под указателем.
 */

export interface DropZone {
  /** Письмо, которому принадлежит поле: чип переносится только между полями одной группы. */
  group: string;
  /** Имя поля внутри письма (его понимает композер). */
  field: string;
}

const zones = new Map<HTMLElement, DropZone>();

/**
 * Что сейчас тянут: `group` — письмо (null — ничего не тянут), `over` — рамка поля под указателем,
 * `index` — перед каким чипом встанет адресат, `markLeft`/`markTop` — где рисовать метку вставки
 * внутри этой рамки.
 */
export const recipientDrag = $state<{ group: string | null; over: HTMLElement | null; index: number; markLeft: number; markTop: number }>({ group: null, over: null, index: 0, markLeft: 0, markTop: 0 });

/** Зарегистрировать рамку поля зоной; возвращает отмену регистрации. */
export function registerZone(el: HTMLElement, zone: DropZone): () => void {
  zones.set(el, zone);
  return () => {
    zones.delete(el);
    if (recipientDrag.over === el) recipientDrag.over = null;
  };
}

export interface ZoneHit extends DropZone {
  el: HTMLElement;
  /** Перед каким чипом встанет адресат; число чипов — в конец. */
  index: number;
  markLeft: number;
  markTop: number;
}

/** Поле группы под точкой окна и место вставки среди его чипов (`[data-chip]`); null — указатель вне полей. */
export function zoneAt(group: string, x: number, y: number): ZoneHit | null {
  for (const [el, zone] of zones) {
    if (zone.group !== group) continue;
    const b = el.getBoundingClientRect();
    if (x < b.left || x > b.right || y < b.top - 3 || y > b.bottom + 3) continue;
    const chips = Array.from(el.querySelectorAll<HTMLElement>("[data-chip]"), (c) => c.getBoundingClientRect());
    // Чипы идут строками слева направо: адресат встаёт перед первым чипом, который ниже указателя
    // или в его строке правее него.
    let index = chips.findIndex((r) => y < r.top || (y <= r.bottom && x < r.left + r.width / 2));
    if (index < 0) index = chips.length;
    const last = chips[chips.length - 1];
    const at = index < chips.length ? { x: chips[index].left - 3, y: chips[index].top } : last ? { x: last.right + 1, y: last.top } : { x: b.left + el.clientLeft + 6, y: b.top + el.clientTop + 3 };
    return { ...zone, el, index, markLeft: at.x - b.left - el.clientLeft, markTop: at.y - b.top - el.clientTop };
  }
  return null;
}
