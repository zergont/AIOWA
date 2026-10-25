//! Письма в тёмных тонах (замечание 6 обкатки): перекраска цветов очищенного HTML письма.
//!
//! Тело остаётся одним HTML: каждый распознанный цвет письма заменяется на
//! `var(--aiowa-dN, исходный цвет)`. Пока переменная не определена (светлый лист, печать и её
//! предпросмотр), браузер берёт исходный цвет, и письмо выглядит ровно как задумано. В тёмном
//! режиме рамка письма (`BodyFrame.svelte`) определяет переменные палитрой [`DarkBody::colors`]:
//! N-й элемент — тёмный вариант переменной `--aiowa-dN`. Вторая копия тела не нужна: тело с
//! `data:`-картинками не уходит в окно дважды, а смена темы перекрашивает письмо без перезагрузки.
//!
//! Что перекрашивается: атрибут `bgcolor` у частей таблицы и `color` у `<font>` (они переносятся
//! в начало `style` той же переменной, так что стиль элемента по-прежнему главнее), а в `style` —
//! `color`, `background-color`, цвет в `background`, `border-color` и `border-*-color`, цвет в
//! `border` и `border-*`. `<style>` письма ammonia вырезает целиком, других таблиц стилей нет.
//! Картинки (`<img>`, `url(…)` в фоне) не трогаются; `transparent`, `inherit`, `currentColor`,
//! полностью прозрачные цвета и прочие ключевые слова остаются как есть.
//!
//! Как (HSL, оттенок и насыщенность сохраняются, меняется светлота):
//! - фон: светлый (L > 0,5) отражается в тёмный, белый становится цветом тёмного листа; тёмный
//!   остаётся;
//! - текст: тёмный (L < 0,5) отражается в светлый, чёрный становится текстом тёмного листа;
//!   светлый остаётся;
//! - рамки: светлота отражается и зажимается в средний диапазон — линии видны, но не режут глаз.
//!
//! Затем контраст: фон элемента затемняется, пока текст на нём (свой или унаследованный) не
//! наберёт 4,5:1, а свой текст элемента осветляется до 4,5:1 к итоговому фону. Унаследованные
//! цвета берутся из стека открытых элементов. Не добирается (полупрозрачный текст) — остаётся
//! лучшее, что вышло.
//!
//! Вход — вывод ammonia (`clean_html`): имена тегов и атрибутов в нижнем регистре, значения в
//! двойных кавычках с экранированием, без комментариев и сырого текста, у пустых элементов нет
//! закрывающих тегов. Всё, кроме изменённых или добавленных атрибутов `style`, копируется байт в
//! байт. HTML с переменными уходит только в окно (показ и печать) и на сервер не отправляется:
//! цитаты и черновики собираются из базы и с сервера заново.

use std::collections::HashMap;

/// Префикс переменных палитры; тот же собирает `darkmail.svelte.ts` окна.
const VAR_PREFIX: &str = "--aiowa-d";
/// Минимальный контраст текста к фону в тёмном режиме (WCAG AA для обычного текста).
const MIN_CONTRAST: f64 = 4.5;
/// Шаг подбора светлоты при добивании контраста.
const STEP: f64 = 0.01;
/// Лист тёмного режима, его текст и ссылки — те же, что в `darkmail.svelte.ts` окна.
const SHEET_BG: [u8; 3] = [0x2b, 0x2b, 0x2b];
const SHEET_TEXT: [u8; 3] = [0xe6, 0xe6, 0xe6];
const SHEET_LINK: [u8; 3] = [0x60, 0xcd, 0xff];
/// Светлота рамок в тёмном режиме: не темнее и не светлее.
const BORDER_L_MIN: f64 = 0.25;
const BORDER_L_MAX: f64 = 0.6;

/// Части таблицы, у которых `bgcolor` красит фон (у остальных элементов атрибут ничего не делает).
const TABLE_PARTS: &[&str] = &["table", "thead", "tbody", "tfoot", "tr", "td", "th", "col", "colgroup"];
/// Пустые элементы: закрывающего тега нет, в стек не кладутся (как в сериализаторе html5ever).
const VOID: &[&str] = &["area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input", "keygen", "link", "meta", "param", "source", "track", "wbr"];

/// Тело письма для показа: HTML с переменными и тёмная палитра к ним.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DarkBody {
    pub(crate) html: String,
    /// `#rrggbb` или `#rrggbbaa`; индекс — номер переменной `--aiowa-dN`.
    pub(crate) colors: Vec<String>,
}

/// Перекрасить очищенный HTML: заменить цвета переменными и собрать их тёмную палитру.
pub(crate) fn darken(html: &str) -> DarkBody {
    let mut out = String::with_capacity(html.len() + html.len() / 16);
    let mut palette = Palette::default();
    let root = Ctx { tag: String::new(), text: Rgba::rgb(SHEET_TEXT), bg: Rgba::rgb(SHEET_BG) };
    let mut stack: Vec<Ctx> = Vec::new();
    let mut rest = 0;
    while let Some(off) = html[rest..].find('<') {
        let lt = rest + off;
        out.push_str(&html[rest..lt]);
        let Some(gt) = tag_end(html, lt) else {
            rest = lt;
            break;
        };
        let raw = &html[lt..=gt];
        match parse_tag(raw) {
            Tag::End(name) => {
                // Вывод ammonia вложен правильно; лишний закрывающий тег стек не трогает.
                if let Some(pos) = stack.iter().rposition(|c| c.tag == name) {
                    stack.truncate(pos);
                }
                out.push_str(raw);
            }
            Tag::Start { name, attrs } => {
                let parent = stack.last().unwrap_or(&root);
                let (rewritten, ctx) = element(raw, &name, &attrs, parent, &mut palette);
                out.push_str(rewritten.as_deref().unwrap_or(raw));
                if !VOID.contains(&name.as_str()) {
                    stack.push(ctx);
                }
            }
            Tag::Other => out.push_str(raw),
        }
        rest = gt + 1;
    }
    out.push_str(&html[rest..]);
    DarkBody { html: out, colors: palette.list }
}

// --- цвет ------------------------------------------------------------------------------------

/// Цвет sRGB с прозрачностью, каналы 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

fn byte(x: f64) -> u8 {
    (x.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl Rgba {
    fn rgb([r, g, b]: [u8; 3]) -> Self {
        Self { r: f64::from(r) / 255.0, g: f64::from(g) / 255.0, b: f64::from(b) / 255.0, a: 1.0 }
    }

    /// Каналы, округлённые до байта: контраст считается по тому, что реально уйдёт в CSS.
    fn quantized(self) -> Self {
        let q = |x: f64| f64::from(byte(x)) / 255.0;
        Self { r: q(self.r), g: q(self.g), b: q(self.b), a: q(self.a) }
    }

    fn hex(self) -> String {
        let (r, g, b, a) = (byte(self.r), byte(self.g), byte(self.b), byte(self.a));
        if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
    }

    /// Этот цвет поверх непрозрачного `bg`.
    fn over(self, bg: Rgba) -> Rgba {
        let a = self.a.clamp(0.0, 1.0);
        Rgba { r: self.r * a + bg.r * (1.0 - a), g: self.g * a + bg.g * (1.0 - a), b: self.b * a + bg.b * (1.0 - a), a: 1.0 }
    }

    /// Относительная яркость по WCAG 2 (прозрачность не учитывается).
    fn luminance(self) -> f64 {
        let lin = |c: f64| if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// Оттенок (0..1), насыщенность и светлота HSL.
    fn hsl(self) -> (f64, f64, f64) {
        let (r, g, b) = (self.r, self.g, self.b);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        if d < 1e-9 {
            return (0.0, 0.0, l);
        }
        let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
        let h = if r >= g && r >= b {
            ((g - b) / d).rem_euclid(6.0)
        } else if g >= b {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h / 6.0, s, l)
    }

    fn from_hsl(h: f64, s: f64, l: f64, a: f64) -> Self {
        let l = l.clamp(0.0, 1.0);
        if s <= 0.0 {
            return Self { r: l, g: l, b: l, a }.quantized();
        }
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        let channel = |t: f64| {
            let t = t.rem_euclid(1.0);
            if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            }
        };
        Self { r: channel(h + 1.0 / 3.0), g: channel(h), b: channel(h - 1.0 / 3.0), a }.quantized()
    }
}

/// Контраст двух непрозрачных цветов по WCAG 2 (1..21).
fn contrast(x: Rgba, y: Rgba) -> f64 {
    let (a, b) = (x.luminance(), y.luminance());
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Текст: тёмный отражается в светлый (чёрный — в текст листа), затем осветляется до контраста с `bg`.
fn dark_text(c: Rgba, bg: Rgba) -> Rgba {
    let (h, s, l0) = c.hsl();
    let top = Rgba::rgb(SHEET_TEXT).hsl().2;
    let mut l = if l0 < 0.5 { top - l0 * (top - 0.5) / 0.5 } else { l0 };
    let mut out = Rgba::from_hsl(h, s, l, c.a);
    while contrast(out.over(bg), bg) < MIN_CONTRAST && l < 1.0 {
        l = (l + STEP).min(1.0);
        out = Rgba::from_hsl(h, s, l, c.a);
    }
    out
}

/// Светлота фона: светлый (L > 0,5) отражается в тёмный, белый — в цвет листа; тёмный остаётся.
fn bg_lightness(l: f64) -> f64 {
    let floor = Rgba::rgb(SHEET_BG).hsl().2;
    if l > 0.5 { 0.5 - (l - 0.5) * (0.5 - floor) / 0.5 } else { l }
}

/// Фон в первом приближении, без подбора контраста.
fn base_bg(c: Rgba) -> Rgba {
    let (h, s, l) = c.hsl();
    Rgba::from_hsl(h, s, bg_lightness(l), c.a)
}

/// Фон: отражение светлоты, затем затемнение, пока `text` на нём не наберёт контраст. `parent` —
/// фон под элементом (для полупрозрачных).
fn dark_bg(c: Rgba, text: Rgba, parent: Rgba) -> Rgba {
    let (h, s, l0) = c.hsl();
    let mut l = bg_lightness(l0);
    let mut out = Rgba::from_hsl(h, s, l, c.a);
    loop {
        let eff = out.over(parent);
        if contrast(text.over(eff), eff) >= MIN_CONTRAST || l <= 0.0 {
            return out;
        }
        l = (l - STEP).max(0.0);
        out = Rgba::from_hsl(h, s, l, c.a);
    }
}

/// Рамка: светлота отражается и зажимается в средний диапазон.
fn dark_border(c: Rgba) -> Rgba {
    let (h, s, l) = c.hsl();
    Rgba::from_hsl(h, s, (1.0 - l).clamp(BORDER_L_MIN, BORDER_L_MAX), c.a)
}

// --- разбор цветов ---------------------------------------------------------------------------

/// Именованные цвета CSS (без `transparent`).
const NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

/// Системные цвета, которые часто ставит Outlook (`color:windowtext`), — в значениях светлой схемы.
const SYSTEM: &[(&str, u32)] = &[
    ("windowtext", 0x000000),
    ("window", 0xffffff),
    ("canvastext", 0x000000),
    ("canvas", 0xffffff),
    ("buttontext", 0x000000),
    ("buttonface", 0xf0f0f0),
    ("graytext", 0x6d6d6d),
    ("highlight", 0x0078d7),
    ("highlighttext", 0xffffff),
];

fn from_u32(v: u32) -> Rgba {
    Rgba::rgb([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn named(name: &str) -> Option<Rgba> {
    NAMED.iter().find(|(n, _)| *n == name).map(|&(_, v)| from_u32(v))
}

fn hex_digit(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa` (без `#`).
fn parse_hex(h: &str) -> Option<Rgba> {
    let d: Vec<u8> = h.bytes().map(hex_digit).collect::<Option<_>>()?;
    let (r, g, b, a) = match d.len() {
        3 | 4 => (d[0] * 17, d[1] * 17, d[2] * 17, d.get(3).map_or(255, |x| x * 17)),
        6 | 8 => (d[0] * 16 + d[1], d[2] * 16 + d[3], d[4] * 16 + d[5], if d.len() == 8 { d[6] * 16 + d[7] } else { 255 }),
        _ => return None,
    };
    Some(Rgba { a: f64::from(a) / 255.0, ..Rgba::rgb([r, g, b]) })
}

/// Число или процент: `scale` — значение для 100 %.
fn number(s: &str, scale: f64) -> Option<f64> {
    let s = s.trim();
    let v = match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<f64>().ok()? / 100.0 * scale,
        None => s.parse::<f64>().ok()?,
    };
    v.is_finite().then_some(v)
}

/// Оттенок в долях оборота: число (градусы) или `deg`, `rad`, `grad`, `turn`.
fn hue(s: &str) -> Option<f64> {
    let s = s.trim();
    let (v, unit) = [("deg", 1.0 / 360.0), ("grad", 1.0 / 400.0), ("rad", 1.0 / std::f64::consts::TAU), ("turn", 1.0)]
        .iter()
        .find_map(|&(u, k)| s.strip_suffix(u).map(|n| (n, k)))
        .unwrap_or((s, 1.0 / 360.0));
    let v = v.trim().parse::<f64>().ok()?;
    v.is_finite().then_some((v * unit).rem_euclid(1.0))
}

/// Аргументы `rgb()`/`hsl()`: через запятые или пробелы с `/` перед прозрачностью.
fn args(inner: &str) -> Vec<&str> {
    if inner.contains(',') { inner.split(',').map(str::trim).collect() } else { inner.split(|c: char| c.is_ascii_whitespace() || c == '/').filter(|p| !p.is_empty()).collect() }
}

fn alpha(parts: &[&str]) -> Option<f64> {
    match parts.get(3) {
        Some(a) => Some(number(a, 1.0)?.clamp(0.0, 1.0)),
        None => Some(1.0),
    }
}

/// Цвет CSS. `None` — не цвет, ключевое слово (`transparent`, `inherit`, `currentColor`…) или
/// полностью прозрачный: такие значения не перекрашиваются.
fn parse_css_color(token: &str) -> Option<Rgba> {
    let t = token.trim().to_ascii_lowercase();
    let c = if let Some(h) = t.strip_prefix('#') {
        parse_hex(h)?
    } else if let Some(inner) = t.strip_prefix("rgba(").or_else(|| t.strip_prefix("rgb(")).and_then(|r| r.strip_suffix(')')) {
        let p = args(inner);
        if p.len() != 3 && p.len() != 4 {
            return None;
        }
        let ch = |i: usize| number(p[i], 255.0).map(|v| v.clamp(0.0, 255.0) / 255.0);
        Rgba { r: ch(0)?, g: ch(1)?, b: ch(2)?, a: alpha(&p)? }
    } else if let Some(inner) = t.strip_prefix("hsla(").or_else(|| t.strip_prefix("hsl(")).and_then(|r| r.strip_suffix(')')) {
        let p = args(inner);
        if p.len() != 3 && p.len() != 4 {
            return None;
        }
        let pc = |i: usize| number(p[i], 100.0).map(|v| v.clamp(0.0, 100.0) / 100.0);
        Rgba::from_hsl(hue(p[0])?, pc(1)?, pc(2)?, alpha(&p)?)
    } else {
        named(&t).or_else(|| SYSTEM.iter().find(|(n, _)| *n == t).map(|&(_, v)| from_u32(v)))?
    };
    (c.a > 0.0).then_some(c)
}

/// Цвет из атрибута (`bgcolor`, `color` у `<font>`) по правилам HTML для устаревших цветов:
/// `bgcolor="fff"` — это `#0f0f0f`, а «chucknorris» — `#c00000`, как в браузере.
fn parse_legacy_color(value: &str) -> Option<Rgba> {
    let v = value.trim_matches(|c: char| c.is_ascii_whitespace());
    if v.is_empty() || v.eq_ignore_ascii_case("transparent") {
        return None;
    }
    if let Some(c) = named(&v.to_ascii_lowercase()) {
        return Some(c);
    }
    if v.len() == 4
        && let Some(h) = v.strip_prefix('#')
        && h.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return parse_hex(h);
    }
    let mut s: String = v.chars().map(|c| if (c as u32) > 0xffff { "00".to_string() } else { c.to_string() }).collect();
    if s.chars().count() > 128 {
        s = s.chars().take(128).collect();
    }
    let s = s.strip_prefix('#').unwrap_or(&s);
    let mut digits: Vec<u8> = s.chars().map(|c| if c.is_ascii_hexdigit() { c as u8 } else { b'0' }).collect();
    while digits.is_empty() || !digits.len().is_multiple_of(3) {
        digits.push(b'0');
    }
    let n = digits.len() / 3;
    let mut parts: Vec<&[u8]> = digits.chunks(n).collect();
    if n > 8 {
        parts = parts.iter().map(|p| &p[n - 8..]).collect();
    }
    let mut len = parts[0].len();
    while len > 2 && parts.iter().all(|p| p[p.len() - len] == b'0') {
        len -= 1;
    }
    let comp = |p: &[u8]| {
        let tail = &p[p.len() - len..];
        let two = &tail[..tail.len().min(2)];
        two.iter().fold(0u8, |acc, &d| acc * 16 + hex_digit(d).unwrap_or(0))
    };
    Some(Rgba::rgb([comp(parts[0]), comp(parts[1]), comp(parts[2])]))
}

// --- разбор тегов ----------------------------------------------------------------------------

enum Tag {
    Start { name: String, attrs: Vec<Attr> },
    End(String),
    /// `<!…>` и прочее, что ammonia не выводит; копируется как есть.
    Other,
}

struct Attr {
    name: String,
    /// Байты значения внутри исходного тега (без кавычек); `None` — атрибут без значения.
    value: Option<(usize, usize)>,
}

/// Индекс `>`, закрывающего тег с `<` в `lt`; кавычки значений атрибутов пропускаются.
fn tag_end(html: &str, lt: usize) -> Option<usize> {
    let mut quote = None;
    for (i, b) in html.as_bytes()[lt + 1..].iter().enumerate() {
        match (quote, b) {
            (Some(q), _) if *b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(*b),
            (None, b'>') => return Some(lt + 1 + i),
            _ => {}
        }
    }
    None
}

fn is_space(b: u8) -> bool {
    b.is_ascii_whitespace()
}

fn parse_tag(raw: &str) -> Tag {
    let b = raw.as_bytes();
    let end = raw.len() - 1;
    if b.get(1) == Some(&b'/') {
        let name = raw[2..end].trim().split(|c: char| c.is_ascii_whitespace()).next().unwrap_or("");
        return Tag::End(name.to_ascii_lowercase());
    }
    if !b.get(1).is_some_and(u8::is_ascii_alphabetic) {
        return Tag::Other;
    }
    let mut i = 1;
    while i < end && !is_space(b[i]) && b[i] != b'/' {
        i += 1;
    }
    let name = raw[1..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while i < end && (is_space(b[i]) || b[i] == b'/') {
            i += 1;
        }
        if i >= end {
            break;
        }
        let ns = i;
        while i < end && !is_space(b[i]) && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let attr_name = raw[ns..i].to_ascii_lowercase();
        while i < end && is_space(b[i]) {
            i += 1;
        }
        let mut value = None;
        if i < end && b[i] == b'=' {
            i += 1;
            while i < end && is_space(b[i]) {
                i += 1;
            }
            if i < end && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = raw[vs..end].bytes().position(|c| c == q).map_or(end, |p| vs + p);
                value = Some((vs, ve));
                i = (ve + 1).min(end);
            } else {
                let vs = i;
                while i < end && !is_space(b[i]) {
                    i += 1;
                }
                value = Some((vs, i));
            }
        }
        if attr_name.is_empty() {
            i += 1;
            continue;
        }
        attrs.push(Attr { name: attr_name, value });
    }
    Tag::Start { name, attrs }
}

/// Значение атрибута как текст: ссылки на символы, которые выводит сериализатор, и числовые.
fn decode_attr(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        let named = [("&amp;", '&'), ("&quot;", '"'), ("&lt;", '<'), ("&gt;", '>'), ("&nbsp;", '\u{a0}'), ("&#39;", '\''), ("&apos;", '\'')];
        if let Some(&(ent, ch)) = named.iter().find(|(e, _)| tail.starts_with(e)) {
            out.push(ch);
            rest = &tail[ent.len()..];
            continue;
        }
        let numeric = tail.strip_prefix("&#").and_then(|t| {
            let semi = t.find(';')?;
            let n = &t[..semi];
            let code = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse::<u32>().ok()?,
            };
            Some((char::from_u32(code)?, semi + 3))
        });
        match numeric {
            Some((ch, used)) => {
                out.push(ch);
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Значение для атрибута в двойных кавычках — как экранирует сериализатор html5ever.
fn encode_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
    out
}

// --- разбор style ----------------------------------------------------------------------------

/// Части `s[from..to]` на верхнем уровне (вне скобок и кавычек) между разделителями `sep`.
fn split_top(s: &str, from: usize, to: usize, sep: impl Fn(char) -> bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = from;
    for (i, c) in s[from..to].char_indices() {
        let i = from + i;
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            c if quote == Some(c) => quote = None,
            _ if quote.is_some() => {}
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && sep(c) => {
                if i > start {
                    out.push((start, i));
                }
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if to > start {
        out.push((start, to));
    }
    out
}

/// Чем свойство красит и сколько у него цветов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Text,
    Bg,
    Border,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Значение — только цвета, от 1 до N (`color`, `border-color`).
    Colors(usize),
    /// Сокращённая запись: цвет среди прочих частей (`background`, `border`).
    Shorthand,
}

fn property(name: &str) -> Option<(Role, Shape)> {
    Some(match name {
        "color" => (Role::Text, Shape::Colors(1)),
        "background-color" => (Role::Bg, Shape::Colors(1)),
        "background" => (Role::Bg, Shape::Shorthand),
        "border-color" => (Role::Border, Shape::Colors(4)),
        "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color" => (Role::Border, Shape::Colors(1)),
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => (Role::Border, Shape::Shorthand),
        _ => return None,
    })
}

/// Цветное объявление `style`: где в строке цвета и что оно значит для фона или текста элемента.
struct Decl {
    role: Role,
    important: bool,
    /// Распознанные цвета: байты внутри строки `style` и сам цвет.
    colors: Vec<(usize, usize, Rgba)>,
    /// Итог для фона или текста: `Some(цвет)`, `None` — прозрачно или неизвестно (как у родителя).
    effect: Option<Rgba>,
}

/// Разбор `style`: цветные объявления и диапазоны чужих `--aiowa-*` (их убираем).
fn parse_style(s: &str) -> (Vec<Decl>, Vec<(usize, usize)>) {
    let mut decls = Vec::new();
    let mut drop = Vec::new();
    for (ds, de) in split_top(s, 0, s.len(), |c| c == ';') {
        let Some(colon) = split_top(s, ds, de, |c| c == ':').first().map(|&(_, e)| e) else { continue };
        if colon >= de {
            continue;
        }
        let name = s[ds..colon].trim().to_ascii_lowercase();
        if name.starts_with("--aiowa-") {
            drop.push((ds, de));
            continue;
        }
        let Some((role, shape)) = property(&name) else { continue };
        let (body_end, important) = split_important(s, colon + 1, de);
        let tokens = split_top(s, colon + 1, body_end, |c| c.is_ascii_whitespace() || c == ',' || c == '/');
        let colors: Vec<(usize, usize, Rgba)> = tokens.iter().filter_map(|&(a, b)| parse_css_color(&s[a..b]).map(|c| (a, b, c))).collect();
        match shape {
            // Неполное или лишнее значение браузер отбросит целиком — такое не трогаем.
            Shape::Colors(max) if tokens.is_empty() || tokens.len() > max || colors.len() != tokens.len() => {
                // `transparent` и ключевые слова — без перекраски, но фон тогда как у родителя.
                decls.push(Decl { role, important, colors: Vec::new(), effect: None });
            }
            _ => {
                let effect = colors.last().map(|&(_, _, c)| c);
                decls.push(Decl { role, important, colors, effect });
            }
        }
    }
    (decls, drop)
}

/// Конец значения без `!important` и признак важности.
fn split_important(s: &str, from: usize, to: usize) -> (usize, bool) {
    let v = s[from..to].trim_end().to_ascii_lowercase();
    if let Some(head) = v.strip_suffix("important")
        && let Some(before) = head.trim_end().strip_suffix('!')
    {
        return (from + before.len(), true);
    }
    (to, false)
}

/// Итог по ролям с учётом `!important`: последнее важное, иначе последнее объявление.
fn winner(decls: &[Decl], role: Role) -> Option<Option<Rgba>> {
    decls.iter().rfind(|d| d.role == role && d.important).or_else(|| decls.iter().rfind(|d| d.role == role)).map(|d| d.effect)
}

// --- перекраска элемента ---------------------------------------------------------------------

/// Цвета, унаследованные потомками открытого элемента (уже тёмные и непрозрачные).
struct Ctx {
    tag: String,
    text: Rgba,
    bg: Rgba,
}

#[derive(Default)]
struct Palette {
    index: HashMap<String, usize>,
    list: Vec<String>,
}

impl Palette {
    /// `var(--aiowa-dN, исходный)`: одинаковые тёмные цвета делят одну переменную.
    fn var(&mut self, dark: Rgba, fallback: &str) -> String {
        let hex = dark.hex();
        let Palette { index, list } = self;
        let n = *index.entry(hex.clone()).or_insert_with(|| {
            list.push(hex);
            list.len() - 1
        });
        format!("var({VAR_PREFIX}{n}, {fallback})")
    }
}

/// Перекрасить открывающий тег: новый текст тега (`None` — без изменений) и цвета для потомков.
fn element(raw: &str, tag: &str, attrs: &[Attr], parent: &Ctx, palette: &mut Palette) -> (Option<String>, Ctx) {
    let attr = |name: &str| attrs.iter().find(|a| a.name == name);
    let value = |a: &Attr| a.value.map(|(s, e)| decode_attr(&raw[s..e]));
    let attr_bg = if TABLE_PARTS.contains(&tag) { attr("bgcolor").and_then(value).as_deref().and_then(parse_legacy_color) } else { None };
    let attr_text = if tag == "font" { attr("color").and_then(value).as_deref().and_then(parse_legacy_color) } else { None };
    let style_attr = attr("style");
    let style = style_attr.and_then(value).unwrap_or_default();
    let (decls, drop) = parse_style(&style);

    // Стиль главнее атрибутов: атрибут решает, только если стиль о роли молчит.
    let own_bg = winner(&decls, Role::Bg).unwrap_or(attr_bg);
    let own_text = winner(&decls, Role::Text).unwrap_or(attr_text);
    // Свой текст сначала светлеет к фону в первом приближении (оттенок фона дороже), а фон
    // темнеет, только если тексту не хватило и белого или текст унаследован.
    let base = own_bg.map(|c| base_bg(c).over(parent.bg)).unwrap_or(parent.bg);
    let probe = own_text.map(|c| dark_text(c, base).over(base)).unwrap_or(parent.text);
    let bg = own_bg.map(|c| dark_bg(c, probe, parent.bg).over(parent.bg)).unwrap_or(parent.bg);
    let text = match own_text {
        Some(c) => dark_text(c, bg).over(bg),
        None if tag == "a" => Rgba::rgb(SHEET_LINK),
        None => parent.text,
    };
    let ctx = Ctx { tag: tag.to_string(), text, bg };

    let mut edits: Vec<(usize, usize, String)> = drop.into_iter().map(|(s, e)| (s, e, String::new())).collect();
    for d in &decls {
        for &(s, e, c) in &d.colors {
            let dark = match d.role {
                Role::Text => dark_text(c, bg),
                Role::Bg => dark_bg(c, probe, parent.bg),
                Role::Border => dark_border(c),
            };
            edits.push((s, e, palette.var(dark, &style[s..e])));
        }
    }
    let mut prefix = Vec::new();
    if let Some(c) = attr_bg {
        prefix.push(format!("background-color: {}", palette.var(dark_bg(c, probe, parent.bg), &Rgba { a: 1.0, ..c }.hex())));
    }
    if let Some(c) = attr_text {
        prefix.push(format!("color: {}", palette.var(dark_text(c, bg), &Rgba { a: 1.0, ..c }.hex())));
    }
    if edits.is_empty() && prefix.is_empty() {
        return (None, ctx);
    }

    edits.sort_by_key(|&(s, _, _)| s);
    let mut new_style = String::with_capacity(style.len() + 64);
    if !prefix.is_empty() {
        new_style.push_str(&prefix.join("; "));
        if !style.trim().is_empty() {
            new_style.push_str("; ");
        }
    }
    let mut at = 0;
    for (s, e, rep) in edits {
        new_style.push_str(&style[at..s]);
        new_style.push_str(&rep);
        at = e;
    }
    new_style.push_str(&style[at..]);

    let encoded = encode_attr(&new_style);
    let out = match style_attr.and_then(|a| a.value) {
        Some((vs, ve)) => format!("{}{encoded}{}", &raw[..vs], &raw[ve..]),
        None => {
            let close = raw.len() - 1;
            let at = if raw[..close].ends_with('/') { close - 1 } else { close };
            format!("{} style=\"{encoded}\"{}", raw[..at].trim_end(), &raw[at..])
        }
    };
    (Some(out), ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Светлый показ: каждая переменная — свой запасной цвет.
    fn light(html: &str) -> String {
        substitute(html, |_, fallback| fallback.to_string())
    }

    /// Тёмный показ: каждая переменная — её цвет из палитры.
    fn dark(body: &DarkBody) -> String {
        substitute(&body.html, |n, _| body.colors[n].clone())
    }

    fn substitute(html: &str, f: impl Fn(usize, &str) -> String) -> String {
        let mut out = String::new();
        let mut rest = html;
        let open = format!("var({VAR_PREFIX}");
        while let Some(p) = rest.find(&open) {
            out.push_str(&rest[..p]);
            let tail = &rest[p + open.len()..];
            let comma = tail.find(", ").unwrap();
            let n: usize = tail[..comma].parse().unwrap();
            // Запасной цвет может сам быть rgb(…): закрывающая скобка var — с учётом вложенности.
            let mut depth = 1;
            let end = tail.char_indices().skip(comma).find_map(|(i, c)| {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                (depth == 0).then_some(i)
            });
            let end = end.unwrap();
            out.push_str(&f(n, &tail[comma + 2..end]));
            rest = &tail[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// Тёмные цвета переменных по порядку появления в HTML.
    fn vars(body: &DarkBody) -> Vec<Rgba> {
        let open = format!("var({VAR_PREFIX}");
        body.html.match_indices(&open).map(|(p, _)| {
            let tail = &body.html[p + open.len()..];
            let n: usize = tail[..tail.find(',').unwrap()].parse().unwrap();
            parse_css_color(&body.colors[n]).unwrap()
        }).collect()
    }

    fn sheet() -> Rgba {
        Rgba::rgb(SHEET_BG)
    }

    #[test]
    fn named_and_hex_colors() {
        let b = darken(r#"<p style="color:black">a</p><p style="color: #000">b</p><p style="COLOR:#000000">c</p>"#);
        assert_eq!(b.colors, vec!["#e6e6e6"], "чёрный текст становится текстом листа, одна переменная на всех");
        assert_eq!(light(&b.html), r#"<p style="color:black">a</p><p style="color: #000">b</p><p style="COLOR:#000000">c</p>"#);
        let b = darken(r#"<div style="background-color:white">a</div><div style="background:#FFF">b</div>"#);
        assert_eq!(b.colors, vec!["#2b2b2b"], "белый фон становится цветом листа");
        let b = darken(r#"<p style="color:#fff">a</p>"#);
        assert_eq!(b.colors, vec!["#ffffff"], "светлый текст остаётся");
        let b = darken(r#"<p style="color:navy;background-color:LightYellow">a</p>"#);
        assert_eq!(b.colors.len(), 2);
        let v = vars(&b);
        assert!(v[0].hsl().2 > 0.5 && v[1].hsl().2 < 0.5, "тёмно-синий текст светлеет, светлый фон темнеет: {:?}", b.colors);
    }

    #[test]
    fn hue_is_kept() {
        for (src, role) in [("#c00000", "color"), ("#1f497d", "color"), ("#ddebf7", "background-color"), ("#fff2cc", "background-color")] {
            let b = darken(&format!(r#"<span style="{role}:{src}">x</span>"#));
            let (h0, s0, _) = parse_css_color(src).unwrap().hsl();
            let (h1, s1, _) = vars(&b)[0].hsl();
            let dh = (h0 - h1).abs().min(1.0 - (h0 - h1).abs());
            assert!(dh < 0.02 && (s0 - s1).abs() < 0.1, "{src}: оттенок {h0:.3}→{h1:.3}, насыщенность {s0:.2}→{s1:.2}");
        }
    }

    #[test]
    fn rgb_rgba_hsl_forms() {
        let b = darken(r#"<p style="color: rgb(0, 0, 0)">a</p><p style="color:rgb(0 0 0 / 50%)">b</p><p style="background-color: rgba(255,255,255,0.5)">c</p>"#);
        assert_eq!(b.colors[0], "#e6e6e6");
        assert!(b.colors[1].len() == 9 && b.colors[1].ends_with("80"), "прозрачность сохраняется: {}", b.colors[1]);
        assert!(b.colors[2].len() == 9 && b.colors[2].ends_with("80"), "{}", b.colors[2]);
        assert_eq!(light(&b.html), r#"<p style="color: rgb(0, 0, 0)">a</p><p style="color:rgb(0 0 0 / 50%)">b</p><p style="background-color: rgba(255,255,255,0.5)">c</p>"#);
        assert_eq!(parse_css_color("hsl(0, 100%, 50%)"), Some(Rgba::rgb([255, 0, 0])));
        assert_eq!(parse_css_color("hsla(120deg 100% 25% / 1)"), Some(Rgba::rgb([0, 128, 0])));
        assert_eq!(parse_css_color("rgb(100%, 0%, 0%)"), Some(Rgba::rgb([255, 0, 0])));
        assert_eq!(parse_css_color("#f008").map(Rgba::hex).as_deref(), Some("#ff000088"));
    }

    #[test]
    fn keywords_and_transparent_are_untouched() {
        let src = r#"<p style="color:inherit;background-color:transparent">a</p><p style="color:currentColor;border-color:initial">b</p><p style="background:rgba(0,0,0,0);color:#0000">c</p><p style="color:var(--x);background:unset">d</p><td bgcolor="transparent">e</td>"#;
        let b = darken(src);
        assert_eq!(b.html, src);
        assert!(b.colors.is_empty());
        assert_eq!(parse_css_color("transparent"), None);
        assert_eq!(parse_css_color("rgba(10, 20, 30, 0)"), None);
        assert_eq!(parse_css_color("#12"), None, "неверный hex");
        assert_eq!(parse_css_color("rgb(1,2)"), None);
    }

    #[test]
    fn no_colors_means_identical_html() {
        let src = r#"<p>Привет&nbsp;&amp; <b>мир</b></p><table border="1"><tbody><tr><td width="50%">x</td></tr></tbody></table><pre style="white-space:pre-wrap;font:inherit;margin:0">a &lt; b</pre><img src="data:image/png;base64,iVBORw0KGgo=" alt="">"#;
        let b = darken(src);
        assert_eq!(b.html, src);
        assert!(b.colors.is_empty());
    }

    #[test]
    fn contrast_is_readable() {
        let texts = ["#000000", "#333333", "#1f497d", "#ff0000", "#0000ff", "#808080", "#c00000", "#ffff00", "#ffffff", "#7f7f7f", "#00b050"];
        let bgs = ["#ffffff", "#ffff00", "#00ffff", "#0078d4", "#003366", "#f2f2f2", "#ff0000", "#92d050", "#ffc000", "#808080", "#000000"];
        for t in texts {
            // Текст на листе.
            let b = darken(&format!(r#"<span style="color:{t}">x</span>"#));
            let c = vars(&b)[0];
            assert!(contrast(c, sheet()) >= MIN_CONTRAST, "{t} на листе: {:.2}", contrast(c, sheet()));
            for bg in bgs {
                for html in [format!(r#"<div style="background-color:{bg}"><span style="color:{t}">x</span></div>"#), format!(r#"<table bgcolor="{bg}"><tbody><tr><td><font color="{t}">x</font></td></tr></tbody></table>"#), format!(r#"<td style="background:{bg};color:{t}">x</td>"#)] {
                    let b = darken(&html);
                    let v = vars(&b);
                    let (bgc, tc) = (v[0], v[1]);
                    let ratio = contrast(tc, bgc);
                    assert!(ratio >= MIN_CONTRAST, "{t} на {bg} ({html}): {:?} → {ratio:.2}", b.colors);
                }
            }
        }
        // Унаследованный текст листа на своём фоне элемента.
        for bg in bgs {
            let b = darken(&format!(r#"<div style="background:{bg}">текст</div>"#));
            let ratio = contrast(Rgba::rgb(SHEET_TEXT), vars(&b)[0]);
            assert!(ratio >= MIN_CONTRAST, "текст листа на {bg}: {ratio:.2}");
        }
        // Унаследованный цвет предка: фон потомка подбирается под него.
        let b = darken(r##"<div style="color:#000"><table bgcolor="#ffff99"><tbody><tr><td>x</td></tr></tbody></table></div>"##);
        let v = vars(&b);
        assert!(contrast(v[0], v[1]) >= MIN_CONTRAST, "{:?}", b.colors);
    }

    #[test]
    fn dark_designs_are_kept() {
        let b = darken(r#"<td style="background:#003366;color:#ffffff">x</td>"#);
        assert_eq!(dark(&b), r#"<td style="background:#003366;color:#ffffff">x</td>"#, "тёмный фон с белым текстом уже читаем");
    }

    #[test]
    fn structure_and_images_are_preserved() {
        let src = r##"<html><head><style>p{color:red}</style></head><body bgcolor="#ffffff">
<div class="WordSection1" style="font-family:&quot;Calibri&quot;,sans-serif;color:#1F497D">
<p style="margin:0">Добрый день!</p>
<table border="0" cellpadding="0" bgcolor="#F2F2F2" style="border-collapse:collapse"><tr>
<td style="border:solid windowtext 1.0pt;background:#DDEBF7;padding:0">ячейка</td>
<td style="background: url(data:image/png;base64,iVBORw0KGgo=) #ffffff no-repeat">фон-картинка</td>
</tr></table>
<blockquote style="border-left:2px solid #ccc">цитата <a href="https://example.ru/" style="color:#0563C1">ссылка</a></blockquote>
<font color="#7f7f7f" size="2">подпись</font>
<img src="data:image/png;base64,iVBORw0KGgo=" width="10" style="border:1px solid #000">
</div></body></html>"##;
        let clean = crate::sanitize_html(src, &Arc::new(crate::InlineMap::new()));
        let b = darken(&clean);
        assert!(!b.colors.is_empty());
        let tags = |h: &str| h.split('<').skip(1).map(|t| t.split([' ', '>']).next().unwrap_or("").to_string()).collect::<Vec<_>>();
        let text = |h: &str| h.split('<').map(|t| t.split_once('>').map_or(t, |(_, x)| x)).collect::<String>();
        assert_eq!(tags(&b.html), tags(&clean), "теги и их порядок те же");
        assert_eq!(text(&b.html), text(&clean), "текст тот же");
        assert_eq!(b.html.matches(r#"src="data:image/png;base64,iVBORw0KGgo=""#).count(), 1, "картинка не тронута");
        assert!(b.html.contains("url(data:image/png;base64,iVBORw0KGgo=) var(--aiowa-d"), "фоновая картинка не тронута: {}", b.html);
        assert!(b.html.contains("font-family:&quot;Calibri&quot;,sans-serif;color:var(--aiowa-d"), "кавычки в style экранированы как были: {}", b.html);
        // Светлый показ — исходные цвета: в стиле те же значения, атрибуты перенесены в начало стиля.
        let light = light(&b.html);
        assert!(light.contains(r##"bgcolor="#F2F2F2" style="background-color: #f2f2f2; border-collapse:collapse""##), "{light}");
        assert!(light.contains(r##"<font color="#7f7f7f" size="2" style="color: #7f7f7f">"##), "{light}");
        assert!(light.contains("border:solid windowtext 1.0pt;background:#DDEBF7"), "{light}");
        let strip = |h: &str| h.replace(" style=\"color: #7f7f7f\"", "").replace("background-color: #f2f2f2; ", "");
        assert_eq!(strip(&light), clean, "кроме перенесённых атрибутов светлый показ совпадает с очищенным письмом");
    }

    #[test]
    fn important_and_attribute_order_are_kept() {
        let b = darken(r#"<p style="color:#333 !important;background-color:#fff!IMPORTANT">x</p>"#);
        assert!(b.html.contains("color:var(--aiowa-d0, #333) !important;background-color:var(--aiowa-d1, #fff)!IMPORTANT"), "{}", b.html);
        // Стиль элемента главнее атрибута: атрибут идёт первым, стиль после него.
        let b = darken(r##"<td bgcolor="#ffffff" style="background-color:#ffff00">x</td>"##);
        assert_eq!(light(&b.html), r##"<td bgcolor="#ffffff" style="background-color: #ffffff; background-color:#ffff00">x</td>"##);
        // У div атрибут bgcolor ничего не красит — и не переносится.
        let src = r##"<div bgcolor="#ffffff">x</div><span color="red">y</span>"##;
        assert_eq!(darken(src).html, src);
        // Неполное значение браузер отбросит — его не трогаем.
        let src = r#"<p style="color:#000 #fff;border-color:red blue green yellow black">x</p>"#;
        assert_eq!(darken(src).html, src);
    }

    #[test]
    fn borders_and_shorthands() {
        let b = darken(r#"<td style="border:solid windowtext 1.0pt;border-color:#ccc #000;border-top:1px dotted rgb(255, 255, 255)">x</td>"#);
        assert_eq!(b.html.matches("var(--aiowa-d").count(), 4, "{}", b.html);
        for c in vars(&b) {
            let l = c.hsl().2;
            assert!((BORDER_L_MIN - 0.01..=BORDER_L_MAX + 0.01).contains(&l), "рамка в среднем диапазоне: {}", c.hex());
        }
        assert!(b.html.contains("border-top:1px dotted var(--aiowa-d"), "{}", b.html);
        assert!(b.html.contains(", rgb(255, 255, 255))"), "{}", b.html);
    }

    #[test]
    fn legacy_attribute_colors() {
        let hex = |v: &str| parse_legacy_color(v).map(Rgba::hex);
        assert_eq!(hex("#fff").as_deref(), Some("#ffffff"));
        assert_eq!(hex("ffffff").as_deref(), Some("#ffffff"));
        assert_eq!(hex(" #FF0000 ").as_deref(), Some("#ff0000"));
        assert_eq!(hex("Red").as_deref(), Some("#ff0000"));
        assert_eq!(hex("fff").as_deref(), Some("#0f0f0f"), "как в браузере: три цифры без # — по одной на канал");
        assert_eq!(hex("chucknorris").as_deref(), Some("#c00000"));
        assert_eq!(hex("transparent"), None);
        assert_eq!(hex(""), None);
    }

    #[test]
    fn own_custom_properties_are_dropped() {
        let b = darken(r#"<p style="--aiowa-d0:red;color:#000">x</p>"#);
        assert!(!b.html.contains("red"), "{}", b.html);
        assert_eq!(light(&b.html), r#"<p style=";color:#000">x</p>"#);
    }

    #[test]
    fn palette_is_plain_hex() {
        let b = darken(r#"<p style="color:rgba(10,20,30,.4);background:hsl(200 50% 90%)">x</p><font color="chucknorris">y</font>"#);
        assert!(!b.colors.is_empty());
        for c in &b.colors {
            let digits = c.strip_prefix('#').unwrap();
            assert!((digits.len() == 6 || digits.len() == 8) && digits.bytes().all(|d| d.is_ascii_hexdigit() && !d.is_ascii_uppercase()), "{c}");
        }
    }

    #[test]
    fn malformed_input_is_copied() {
        for src in ["<p style=\"color:#000\"", "a < b", "<>", "</>", "<!x>", "<p style='color:red'>x</p>", "<td bgcolor=fff>x</td>"] {
            let b = darken(src);
            assert_eq!(light(&b.html).replace("background-color: #0f0f0f; ", "").replace(" style=\"background-color: #0f0f0f\"", ""), src, "{src}");
        }
    }
}
