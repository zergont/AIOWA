//! Запрос на обычном языке (`docs/ai-layer-design.md` § 5.2, шаг И3): «письма от диспетчера про
//! ремонт за октябрь» разбирается кодом, без модели, в те же условия, что и операторы поиска
//! (`query`): отправитель и получатели по корреспондентам ящика, период, вложения, папка,
//! состояние письма; остальное — слова поиска со своими падежными формами (`morph`).
//!
//! Что понято, показывается чипами: у каждого — место в исходном запросе и оператор, которым его
//! можно заменить. Правка чипа в окне — это правка текста запроса, скрытого состояния нет.
//! Слово в кавычках и явные операторы разбор не трогает.

use crate::morph;
use crate::names::{self, PeopleIndex, PersonMatch};
use crate::nldate::{self, Period};
use crate::query::{self, AttachmentKind, RawToken, SearchQuery, Term};
use crate::text::{fts_norm, norm_lower};
use jiff::civil::Date;
use jiff::tz::TimeZone;

/// Что известно о ящике и о сегодняшнем дне.
pub struct Context<'a> {
    pub today: Date,
    pub tz: TimeZone,
    pub people: &'a PeopleIndex,
    /// Свои адреса: «я писал», «мои письма», «от меня».
    pub own: &'a [String],
    /// Папки ящика: имя и служебное имя (`inbox`, `sentitems`…).
    pub folders: &'a [(String, Option<String>)],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    From,
    To,
    With,
    Period,
    Attachment,
    Folder,
    Unread,
    Flagged,
    Subject,
}

impl ChipKind {
    pub fn code(self) -> &'static str {
        match self {
            ChipKind::From => "from",
            ChipKind::To => "to",
            ChipKind::With => "with",
            ChipKind::Period => "period",
            ChipKind::Attachment => "attachment",
            ChipKind::Folder => "folder",
            ChipKind::Unread => "unread",
            ChipKind::Flagged => "flagged",
            ChipKind::Subject => "subject",
        }
    }
}

/// Другой вариант условия: чем заменить текст чипа в запросе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipOption {
    pub label: String,
    pub operator: String,
}

/// Условие, понятое из слов запроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub kind: ChipKind,
    /// «от: Иванов Иван Петрович», «период: октябрь 2026».
    pub label: String,
    /// Место в исходном запросе в единицах UTF-16 (индексы строки в окне): начало и конец.
    pub start: usize,
    pub end: usize,
    /// То же условие оператором поиска.
    pub operator: String,
    /// Другие варианты: люди с тем же именем, «искать как слова».
    pub options: Vec<ChipOption>,
    /// Пояснение: «ещё 2 с таким именем».
    pub note: Option<String>,
}

/// Как понят запрос.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Understood {
    pub query: SearchQuery,
    pub chips: Vec<Chip>,
    /// Запрос операторами: им можно пользоваться и дальше.
    pub canonical: String,
    /// Слова, которые разбор не понял (кандидаты на доводку моделью).
    pub unknown: Vec<String>,
    /// Тот же запрос без условия по человеку, со словами его имени в тексте, и пометка к нему:
    /// добавляется к выдаче, когда по человеку нашлось слишком мало.
    pub relaxed: Option<(SearchQuery, String)>,
}

impl Understood {
    /// Запрос как набран: только операторы и слова, без разбора фразы.
    pub fn literal(input: &str) -> Self {
        Self { query: query::parse(input), chips: Vec::new(), canonical: input.trim().to_string(), unknown: Vec::new(), relaxed: None }
    }
}

/// Слово оператора или фразы в кавычках: в разборе фразы не участвует.
const OPAQUE: &str = "\u{1}";
/// Людей в одном условии самое большее.
const PEOPLE_MAX: usize = 20;
/// Вариантов в чипе человека.
const OPTIONS_MAX: usize = 6;

const LEAD: &[&str] = &["найди", "найти", "найдите", "покажи", "показать", "покажите", "ищи", "искать", "поищи", "выведи", "дай", "дайте", "нужны", "нужно", "нужен", "нужна", "хочу", "посмотреть", "открой", "все", "всю", "всех"];
const MAIL: &[&str] = &["письмо", "письма", "писем", "письмах", "письму", "сообщение", "сообщения", "сообщений", "почта", "почту", "почты"];
const GLUE: &[&str] = &["который", "которые", "которое", "которая", "которых", "которым", "которую", "где", "что", "чтобы", "это", "мне", "нам", "там", "было", "были", "была", "был", "же", "ли", "бы", "пожалуйста"];
const STOP: &[&str] = &["и", "а", "но", "или", "в", "во", "на", "с", "со", "к", "ко", "у", "о", "об", "обо", "от", "до", "по", "за", "из", "для", "при", "под", "над", "без", "не", "ни", "то"];
const QUESTION: &[&str] = &["когда", "кто", "кого", "кому", "сколько", "какой", "какая", "какие", "какое", "каком", "какую", "почему", "зачем", "куда", "откуда", "чей", "как"];
const WRITE_VERBS: &[&str] = &[
    "писал", "писала", "писали", "написал", "написала", "написали", "пишет", "пишут", "прислал", "прислала", "прислали", "присылал", "присылала", "присылали", "отправил", "отправила", "отправили", "отправлял", "отправляла", "отправляли", "направил",
    "направила", "направили", "направлял", "направляла", "переслал", "переслала", "переслали", "пересылал", "пересылала", "скинул", "скинула", "скинули", "сообщил", "сообщила", "сообщили", "сообщал", "сообщала", "ответил", "ответила", "ответили",
    "отвечал", "отвечала",
];

/// Слова, которые говорят о времени, но периодом не стали: в поиск словами не идут.
fn is_date_hint(w: &str) -> bool {
    ["недел", "месяц", "квартал", "полугоди", "прошл", "позапрошл", "праздник", "майск", "выходны"].iter().any(|p| w.starts_with(p)) || matches!(w, "утром" | "вечером" | "ночью" | "давно")
}

fn clean(value: &str) -> String {
    norm_lower(value).trim_matches(|c: char| !c.is_alphanumeric()).to_string()
}

/// Слово может быть частью имени человека.
fn mentionable(w: &str) -> bool {
    w != OPAQUE && w.chars().count() >= 2 && w.chars().all(|c| c.is_alphabetic() || c == '-') && !STOP.contains(&w) && !GLUE.contains(&w) && !WRITE_VERBS.contains(&w) && !QUESTION.contains(&w)
}

/// Человек в начале слов: до трёх слов, самое длинное сочетание, по которому кто-то нашёлся.
fn mention(rest: &[&str], people: &PeopleIndex) -> Option<(usize, Vec<PersonMatch>)> {
    let max = rest.iter().take(3).take_while(|w| mentionable(w)).count();
    (1..=max).rev().find_map(|k| {
        let words: Vec<String> = rest[..k].iter().map(|w| w.to_string()).collect();
        let found = people.resolve(&words);
        (!found.is_empty()).then_some((k, found))
    })
}

/// «с вложением», «с таблицей»: вложение вообще или его вид.
fn attachment_word(w: &str) -> Option<Option<AttachmentKind>> {
    Some(match w {
        "вложением" | "вложениями" | "вложенным" | "вложенными" | "прикрепленным" | "прикрепленными" | "аттачем" | "аттачами" | "файлом" | "файлами" | "сканом" | "сканами" | "скрепкой" => None,
        "pdf" | "пдф" | "пдфкой" => Some(AttachmentKind::Pdf),
        "таблицей" | "таблицами" | "excel" | "эксель" | "экселем" | "xlsx" | "xls" => Some(AttachmentKind::Sheet),
        "word" | "ворд" | "вордом" | "docx" | "doc" => Some(AttachmentKind::Doc),
        "фото" | "фотографией" | "фотографиями" | "фоткой" | "фотками" | "картинкой" | "картинками" | "изображением" | "изображениями" | "снимком" | "снимками" => Some(AttachmentKind::Image),
        "архивом" | "архивами" | "zip" | "rar" => Some(AttachmentKind::Archive),
        "презентацией" | "презентациями" | "pptx" => Some(AttachmentKind::Slides),
        _ => return None,
    })
}

/// Служебная папка по слову после «в»: «во входящих», «в отправленных».
fn folder_word(w: &str) -> Option<&'static str> {
    Some(match w {
        "входящих" | "входящие" => "inbox",
        "отправленных" | "отправленные" => "sentitems",
        "черновиках" | "черновиков" | "черновики" => "drafts",
        "удаленных" | "удаленные" | "корзине" | "корзины" => "deleteditems",
        "нежелательной" | "нежелательных" | "спаме" | "спама" => "junkemail",
        _ => return None,
    })
}

fn folder_default_name(code: &str) -> &'static str {
    match code {
        "inbox" => "Входящие",
        "sentitems" => "Отправленные",
        "drafts" => "Черновики",
        "deleteditems" => "Удалённые",
        _ => "Нежелательная почта",
    }
}

/// Папка по словам её имени после «в папке»: каждое слово запроса — начало слова имени (с учётом
/// падежа). Сколько слов заняло имя и имя папки.
fn folder_by_name(rest: &[&str], folders: &[(String, Option<String>)]) -> Option<(usize, String)> {
    let max = rest.iter().take(3).take_while(|w| !w.is_empty() && **w != OPAQUE && !STOP.contains(w)).count();
    (1..=max).rev().find_map(|k| {
        let words = &rest[..k];
        let fits = |name: &str| {
            let name_words: Vec<String> = norm_lower(name).split(|c: char| !c.is_alphanumeric()).filter(|x| !x.is_empty()).map(str::to_string).collect();
            words.iter().all(|w| {
                let base = morph::stem(w).map(|(s, _)| s).unwrap_or(w);
                name_words.iter().any(|n| n == w || n.starts_with(base))
            })
        };
        // Имя из стольких же слов — точнее, чем просто первое подходящее.
        let mut found: Vec<&String> = folders.iter().map(|(name, _)| name).filter(|name| fits(name)).collect();
        found.sort_by_key(|name| name.split_whitespace().count().abs_diff(k));
        found.first().map(|name| (k, (*name).clone()))
    })
}

/// Смещения символов запроса в единицах UTF-16.
fn utf16_offsets(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    let mut at = 0;
    for c in chars {
        out.push(at);
        at += c.len_utf16();
    }
    out.push(at);
    out
}

/// Значение оператора: в кавычках, если в нём пробелы.
fn operator_value(value: &str) -> String {
    if value.contains(char::is_whitespace) { format!("\"{}\"", value.replace('"', "")) } else { value.to_string() }
}

/// Другие формы слова для поиска. Фамилия из ящика в начальной форме («иванов») не разбирается:
/// иначе её основа совпала бы с именем («иван»); к имени не добавляется фамилия («ивана» → «иванов»).
fn add_forms(q: &mut SearchQuery, word: &str, people: &PeopleIndex) {
    let key = norm_lower(word);
    if q.forms.contains_key(&key) {
        return;
    }
    let Some((base, ending)) = morph::stem(&key) else { return };
    if matches!(ending, "ов" | "ев") && people.has_name_word(&key) {
        return;
    }
    let surnames = [format!("{base}ов"), format!("{base}ев")];
    let forms: Vec<String> = morph::forms(&key).into_iter().filter(|f| !(surnames.contains(f) && people.has_name_word(f))).collect();
    if !forms.is_empty() {
        q.forms.insert(key, forms);
    }
}

/// Сколько найденных людей идёт в условие: одно личное имя («от Васи») — самый частый
/// корреспондент с этим именем; фамилия, должность или несколько слов — все, кто подошёл.
fn chosen_count(words: &[&str], found: &[PersonMatch]) -> usize {
    let first_name_only = words.len() == 1 && names::name_words(words[0]).iter().any(|w| names::is_first_name(w));
    if first_name_only { 1 } else { found.len().min(PEOPLE_MAX) }
}

/// Адреса людей в нижнем регистре, без повторов.
fn emails_of(people: &[PersonMatch]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in people.iter().flat_map(|p| &p.emails) {
        let e = e.to_lowercase();
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Условия поиска, названные моделью при доводке запроса (`docs/ai-layer-design.md` § 5.3), уже
/// проверенные: люди — словами, как в запросе; адреса по ним подбирает [`compose`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFields {
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub with: Vec<String>,
    pub from_me: bool,
    pub period: Option<Period>,
    pub has_attachment: bool,
    pub attachment_kind: Option<AttachmentKind>,
    /// Служебное имя папки: `inbox`, `sentitems`, `drafts`, `deleteditems`, `junkemail`.
    pub folder: Option<String>,
    pub unread: bool,
    pub flagged: bool,
    pub words: Vec<String>,
    pub phrases: Vec<String>,
}

/// Условия модели операторами поиска и подписи к ним.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composed {
    pub operators: String,
    pub labels: Vec<String>,
}

/// Собрать условия модели в запрос операторами — тот же вид, что у `Understood::canonical`: его
/// видно, им можно заменить текст в строке поиска и править дальше. Человек, которого нет среди
/// корреспондентов, ищется словами имени в отправителе или получателях.
pub fn compose(f: &ModelFields, ctx: &Context) -> Composed {
    let mut ops: Vec<String> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    if f.from_me && !ctx.own.is_empty() {
        ops.push(format!("от:{}", ctx.own.iter().map(|e| e.to_lowercase()).collect::<Vec<_>>().join(",")));
        labels.push("от: меня".into());
    }
    for (people, op, prefix) in [(&f.from, "от", "от"), (&f.to, "кому", "кому"), (&f.with, "переписка", "переписка с")] {
        for name in people {
            let typed = name.replace('"', "").trim().to_string();
            if typed.is_empty() {
                continue;
            }
            if typed.contains('@') && !typed.contains(char::is_whitespace) {
                ops.push(format!("{op}:{}", typed.to_lowercase()));
                labels.push(format!("{prefix}: {typed}"));
                continue;
            }
            let words: Vec<String> = norm_lower(&typed).split(|c: char| !(c.is_alphanumeric() || c == '-')).filter(|w| !w.is_empty()).map(str::to_string).collect();
            let found = ctx.people.resolve(&words);
            if found.is_empty() {
                ops.push(format!("{op}:{}", operator_value(&typed)));
                labels.push(format!("{prefix}: {typed}"));
            } else {
                let refs: Vec<&str> = words.iter().map(String::as_str).collect();
                let n = chosen_count(&refs, &found);
                ops.push(format!("{op}:{}", emails_of(&found[..n]).join(",")));
                labels.push(if n > 1 { format!("{prefix}: {} и ещё {}", found[0].label, n - 1) } else { format!("{prefix}: {}", found[0].label) });
            }
        }
    }
    if let Some(p) = f.period.filter(|p| p.start.is_some() || p.end.is_some()) {
        ops.push(nldate::operators(&p));
        labels.push(format!("период: {}", nldate::label(&p)));
    }
    match f.attachment_kind {
        Some(kind) => {
            ops.push(format!("вложение:{}", kind.code()));
            labels.push(format!("вложение: {}", kind.label()));
        }
        None if f.has_attachment => {
            ops.push("вложение:да".into());
            labels.push("с вложениями".into());
        }
        None => {}
    }
    if let Some(code) = f.folder.as_deref().filter(|c| matches!(*c, "inbox" | "sentitems" | "drafts" | "deleteditems" | "junkemail")) {
        let name = ctx.folders.iter().find(|(_, d)| d.as_deref() == Some(code)).map(|(name, _)| name.clone()).unwrap_or_else(|| folder_default_name(code).to_string());
        ops.push(format!("папка:{code}"));
        labels.push(format!("папка: {name}"));
    }
    if f.unread {
        ops.push("прочитано:нет".into());
        labels.push("непрочитанные".into());
    }
    if f.flagged {
        ops.push("флаг:да".into());
        labels.push("с флагом".into());
    }
    // Слово не должно стать оператором или фразой: двоеточия и кавычки из него убираются.
    let words: Vec<String> = f.words.iter().map(|w| w.replace([':', '"', '«', '»'], " ").split_whitespace().collect::<Vec<_>>().join(" ")).filter(|w| !w.is_empty()).collect();
    if !words.is_empty() {
        labels.push(format!("слова: {}", words.join(" ")));
        ops.extend(words);
    }
    for phrase in f.phrases.iter().map(|p| p.replace(['"', '«', '»'], " ").split_whitespace().collect::<Vec<_>>().join(" ")).filter(|p| !p.is_empty()) {
        labels.push(format!("фраза: «{phrase}»"));
        ops.push(format!("\"{phrase}\""));
    }
    Composed { operators: ops.join(" "), labels }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Кусок остаётся в запросе как есть: оператор, фраза, слово поиска.
    Keep,
    /// Служебное слово: в операторах его нет.
    Drop,
    /// Начало чипа с этим номером.
    Chip(usize),
    /// Продолжение чипа.
    Cont,
}

/// Условие по человеку из слов запроса.
struct PersonFilter {
    kind: ChipKind,
    emails: Vec<String>,
    /// Слова запроса, которыми назван человек; пусто — это сам автор.
    mention: Vec<String>,
}

/// Чип без места в запросе: вид, подпись и оператор; варианты и пояснение — по необходимости.
struct ChipSpec {
    kind: ChipKind,
    label: String,
    operator: String,
    options: Vec<ChipOption>,
    note: Option<String>,
    /// Добавить вариант «искать как слова».
    literal: bool,
}

impl ChipSpec {
    fn new(kind: ChipKind, label: impl Into<String>, operator: impl Into<String>) -> Self {
        Self { kind, label: label.into(), operator: operator.into(), options: Vec::new(), note: None, literal: true }
    }

    /// Без варианта «искать как слова»: условие и так записано словами запроса.
    fn exact(mut self) -> Self {
        self.literal = false;
        self
    }
}

struct Scan<'a> {
    ctx: &'a Context<'a>,
    tokens: Vec<RawToken>,
    chars: Vec<char>,
    utf16: Vec<usize>,
    slots: Vec<Slot>,
    chips: Vec<Chip>,
    filters: Vec<PersonFilter>,
}

impl Scan<'_> {
    /// Текст запроса под кусками `a..=b`.
    fn typed(&self, a: usize, b: usize) -> String {
        self.chars[self.tokens[a].start..self.tokens[b].end].iter().collect()
    }

    /// «Искать как слова»: слова кусков `a..=b` в кавычках, без предлогов.
    fn literal(&self, a: usize, b: usize) -> Option<ChipOption> {
        let words: Vec<String> = (a..=b).filter(|i| !STOP.contains(&clean(&self.tokens[*i].value).as_str())).map(|i| self.tokens[i].value.trim_matches(|c: char| !c.is_alphanumeric()).to_string()).filter(|w| !w.is_empty()).collect();
        (!words.is_empty()).then(|| ChipOption { label: "искать как слова".into(), operator: words.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" ") })
    }

    /// Чип на кусках `a..=b`.
    fn chip(&mut self, a: usize, b: usize, spec: ChipSpec) {
        let ChipSpec { kind, label, operator, mut options, note, literal } = spec;
        if literal && let Some(l) = self.literal(a, b) {
            options.push(l);
        }
        self.slots[a] = Slot::Chip(self.chips.len());
        for s in &mut self.slots[a + 1..=b] {
            *s = Slot::Cont;
        }
        self.chips.push(Chip { kind, label, start: self.utf16[self.tokens[a].start], end: self.utf16[self.tokens[b].end], operator, options, note });
    }

    fn drop(&mut self, a: usize, b: usize) {
        for s in &mut self.slots[a..=b] {
            *s = Slot::Drop;
        }
    }

    /// Чип человека на кусках `a..=b`; `words` — слова, которыми он назван.
    fn person(&mut self, a: usize, b: usize, kind: ChipKind, words: &[&str], found: Vec<PersonMatch>) {
        let (prefix, op) = match kind {
            ChipKind::To => ("кому", "кому"),
            ChipKind::With => ("переписка с", "переписка"),
            _ => ("от", "от"),
        };
        let chosen = chosen_count(words, &found);
        let emails = emails_of(&found[..chosen]);
        let label = if chosen > 1 { format!("{prefix}: {} и ещё {}", found[0].label, chosen - 1) } else { format!("{prefix}: {}", found[0].label) };
        let mut options: Vec<ChipOption> = Vec::new();
        if found.len() > 1 {
            for (i, p) in found.iter().enumerate().take(OPTIONS_MAX) {
                if chosen == 1 && i == 0 {
                    continue;
                }
                options.push(ChipOption { label: p.label.clone(), operator: format!("{op}:{}", emails_of(std::slice::from_ref(p)).join(",")) });
            }
            if chosen == 1 {
                let all = &found[..found.len().min(PEOPLE_MAX)];
                options.push(ChipOption { label: format!("все с таким именем ({})", all.len()), operator: format!("{op}:{}", emails_of(all).join(",")) });
            }
        }
        let note = (chosen == 1 && found.len() > 1).then(|| format!("ещё {} с таким именем", found.len() - 1));
        self.chip(a, b, ChipSpec { options, note, ..ChipSpec::new(kind, label, format!("{op}:{}", emails.join(","))) });
        self.filters.push(PersonFilter { kind, emails, mention: words.iter().map(|w| w.to_string()).collect() });
    }

    /// «от меня», «я писал», «мои письма» на кусках `a..=b`.
    fn me(&mut self, a: usize, b: usize) {
        if self.ctx.own.is_empty() {
            self.drop(a, b);
            return;
        }
        let emails: Vec<String> = self.ctx.own.iter().map(|e| e.to_lowercase()).collect();
        self.chip(a, b, ChipSpec::new(ChipKind::From, "от: меня", format!("от:{}", emails.join(","))).exact());
        self.filters.push(PersonFilter { kind: ChipKind::From, emails, mention: Vec::new() });
    }
}

fn intersect(a: Option<Period>, b: Period) -> Period {
    let Some(a) = a else { return b };
    Period {
        start: match (a.start, b.start) {
            (Some(x), Some(y)) => Some(x.max(y)),
            (x, y) => x.or(y),
        },
        end: match (a.end, b.end) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        },
    }
}

/// Разобрать запрос: операторы и фразы — как набраны, слова — по правилам обычного языка.
pub fn understand(input: &str, ctx: &Context) -> Understood {
    let tokens = query::split_tokens(input);
    let chars: Vec<char> = input.chars().collect();
    let norm: Vec<String> = tokens.iter().map(|t| if t.key.is_some() || t.quoted { OPAQUE.to_string() } else { clean(&t.value) }).collect();
    let w: Vec<&str> = norm.iter().map(String::as_str).collect();
    let n = tokens.len();
    let mut s = Scan { ctx, utf16: utf16_offsets(&chars), chars, slots: vec![Slot::Keep; n], chips: Vec::new(), filters: Vec::new(), tokens };
    // Условия без людей из фразы: к ним добавляются либо адреса людей, либо их имена словами.
    let mut base = SearchQuery::default();
    // Куски, которые станут словами поиска.
    let mut pending: Vec<usize> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut period: Option<Period> = None;
    let mut lead = true;
    let word = |i: usize| w.get(i).copied().unwrap_or("");

    let mut i = 0;
    while i < n {
        if s.tokens[i].key.is_some() || s.tokens[i].quoted {
            query::apply_token(&mut base, &s.tokens[i]);
            lead = false;
            i += 1;
            continue;
        }
        let (w0, w1, w2) = (w[i], word(i + 1), word(i + 2));
        if w0.is_empty() {
            s.drop(i, i);
            i += 1;
            continue;
        }
        // «найди все письма …»: вводные слова в начале запроса.
        if lead && i + 1 < n && (LEAD.contains(&w0) || MAIL.contains(&w0)) {
            lead = !MAIL.contains(&w0);
            s.drop(i, i);
            i += 1;
            continue;
        }
        lead = false;

        // Период: раньше отправителя, чтобы «от 15 октября» осталось датой.
        if let Some((k, p)) = nldate::parse(&w[i..], ctx.today) {
            period = Some(intersect(period, p));
            s.chip(i, i + k - 1, ChipSpec::new(ChipKind::Period, format!("период: {}", nldate::label(&p)), nldate::operators(&p)));
            i += k;
            continue;
        }

        // «от меня», «от Иванова», «от диспетчера».
        if w0 == "от" {
            if w1 == "меня" {
                s.me(i, i + 1);
                i += 2;
                continue;
            }
            if let Some((k, found)) = mention(&w[(i + 1).min(n)..], ctx.people) {
                s.person(i, i + k, ChipKind::From, &w[i + 1..i + 1 + k], found);
                i += 1 + k;
                continue;
            }
            // Не человек из ящика: «от» уходит, слово остаётся словом поиска.
            if mentionable(w1) {
                unknown.push(w1.to_string());
            }
            s.drop(i, i);
            i += 1;
            continue;
        }

        // «я писал Иванову», «мои письма Петрову».
        if (w0 == "я" && WRITE_VERBS.contains(&w1)) || (matches!(w0, "мои" | "мое" | "мою") && matches!(w1, "письма" | "письмо" | "сообщения" | "сообщение" | "ответы" | "ответ" | "отправленные")) {
            s.me(i, i + 1);
            i += 2;
            if let Some((k, found)) = mention(&w[i.min(n)..], ctx.people) {
                s.person(i, i + k - 1, ChipKind::To, &w[i..i + k], found);
                i += k;
            }
            continue;
        }

        // «Вася писал …», «что прислал Иванов».
        if WRITE_VERBS.contains(&w0) {
            // Слова перед глаголом, если они стоят вплотную и ещё не разобраны.
            let mut back = 0;
            while back < 3 && back < pending.len() && pending[pending.len() - 1 - back] + back + 1 == i && mentionable(w[i - 1 - back]) {
                back += 1;
            }
            let before = (1..=back).rev().find_map(|k| {
                let words: Vec<String> = w[i - k..i].iter().map(|x| x.to_string()).collect();
                let found = ctx.people.resolve(&words);
                (!found.is_empty()).then_some((k, found))
            });
            if let Some((k, found)) = before {
                pending.truncate(pending.len() - k);
                s.person(i - k, i, ChipKind::From, &w[i - k..i], found);
                i += 1;
            } else if let Some((k, found)) = mention(&w[(i + 1).min(n)..], ctx.people) {
                s.person(i, i + k, ChipKind::From, &w[i + 1..i + 1 + k], found);
                i += 1 + k;
            } else {
                s.drop(i, i);
                i += 1;
            }
            continue;
        }

        // «переписка с Ивановым».
        if matches!(w0, "переписка" | "переписку" | "переписке" | "переписки" | "общение" | "диалог") {
            if matches!(w1, "с" | "со")
                && let Some((k, found)) = mention(&w[(i + 2).min(n)..], ctx.people)
            {
                s.person(i, i + 1 + k, ChipKind::With, &w[i + 2..i + 2 + k], found);
                i += 2 + k;
            } else {
                s.drop(i, i);
                i += 1;
            }
            continue;
        }

        // «с вложением», «с таблицей», «есть вложения».
        let attachment = if matches!(w0, "с" | "со") {
            attachment_word(w1).map(|kind| (kind, if matches!(w1, "вложенным" | "вложенными" | "прикрепленным" | "прикрепленными") && matches!(w2, "файлом" | "файлами" | "документом" | "документами") { 3 } else { 2 }))
        } else if w0 == "есть" && matches!(w1, "вложение" | "вложения" | "файл" | "файлы") {
            Some((None, 2))
        } else {
            None
        };
        if let Some((kind, k)) = attachment {
            base.has_attachment = true;
            let (label, operator) = match kind {
                Some(kind) => {
                    if !base.attachment_kinds.contains(&kind) {
                        base.attachment_kinds.push(kind);
                    }
                    (format!("вложение: {}", kind.label()), format!("вложение:{}", kind.code()))
                }
                None => ("с вложениями".to_string(), "вложение:да".to_string()),
            };
            s.chip(i, i + k - 1, ChipSpec::new(ChipKind::Attachment, label, operator));
            i += k;
            continue;
        }

        // «во входящих», «в папке Проекты», «отправленные».
        if matches!(w0, "в" | "во" | "из") {
            if let Some(code) = folder_word(w1) {
                let k = if code == "junkemail" && matches!(w2, "почте" | "почты") { 3 } else { 2 };
                let name = ctx.folders.iter().find(|(_, d)| d.as_deref() == Some(code)).map(|(name, _)| name.clone()).unwrap_or_else(|| folder_default_name(code).to_string());
                base.folder = Some(code.to_string());
                s.chip(i, i + k - 1, ChipSpec::new(ChipKind::Folder, format!("папка: {name}"), format!("папка:{code}")));
                i += k;
                continue;
            }
            if matches!(w1, "папке" | "папки" | "папку")
                && let Some((k, name)) = folder_by_name(&w[(i + 2).min(n)..], ctx.folders)
            {
                base.folder = Some(fts_norm(&name));
                s.chip(i, i + 1 + k, ChipSpec::new(ChipKind::Folder, format!("папка: {name}"), format!("папка:{}", operator_value(&name))));
                i += 2 + k;
                continue;
            }
        }
        if matches!(w0, "входящие" | "отправленные" | "черновики" | "удаленные")
            && let Some(code) = folder_word(w0)
        {
            let name = ctx.folders.iter().find(|(_, d)| d.as_deref() == Some(code)).map(|(name, _)| name.clone()).unwrap_or_else(|| folder_default_name(code).to_string());
            base.folder = Some(code.to_string());
            s.chip(i, i, ChipSpec::new(ChipKind::Folder, format!("папка: {name}"), format!("папка:{code}")));
            i += 1;
            continue;
        }

        // «непрочитанные», «не прочитанные», «с флагом», «помеченные».
        let unread = if w0.starts_with("непрочитанн") {
            Some(1)
        } else if w0 == "не" && w1.starts_with("прочитан") {
            Some(2)
        } else {
            None
        };
        if let Some(k) = unread {
            base.unread = Some(true);
            s.chip(i, i + k - 1, ChipSpec::new(ChipKind::Unread, "непрочитанные", "прочитано:нет"));
            i += k;
            continue;
        }
        let flag_word = |x: &str| matches!(x, "флагом" | "флажком" | "флагами" | "флажками");
        let flagged = if matches!(w0, "с" | "со") && flag_word(w1) {
            Some(2)
        } else if matches!(w0, "помеченные" | "помеченных" | "отмеченные" | "отмеченных") {
            Some(1 + flag_word(w1) as usize)
        } else {
            None
        };
        if let Some(k) = flagged {
            base.flagged = Some(true);
            s.chip(i, i + k - 1, ChipSpec::new(ChipKind::Flagged, "с флагом", "флаг:да"));
            i += k;
            continue;
        }

        // «в теме договор поставки», «с темой счёт».
        if matches!(w0, "в" | "с") && matches!(w1, "теме" | "темой") {
            let mut k = 0;
            while k < 5 && i + 2 + k < n {
                let x = w[i + 2 + k];
                if x.is_empty() || x == OPAQUE || STOP.contains(&x) || GLUE.contains(&x) || nldate::parse(&w[i + 2 + k..], ctx.today).is_some() {
                    break;
                }
                k += 1;
            }
            if k > 0 {
                let text = fts_norm(&s.typed(i + 2, i + 1 + k));
                for x in query::plain_words(&text) {
                    add_forms(&mut base, x, ctx.people);
                }
                s.chip(i, i + 1 + k, ChipSpec::new(ChipKind::Subject, format!("в теме: {text}"), format!("тема:{}", operator_value(&text))).exact());
                base.subject.push(text);
                i += 2 + k;
                continue;
            }
        }

        // «про ремонт», «по поводу договора», «на тему отпуска»: предлог темы в поиск не идёт.
        if i + 1 < n && matches!(w0, "про" | "насчет" | "касательно" | "относительно") {
            s.drop(i, i);
            i += 1;
            continue;
        }
        if i + 2 < n && matches!((w0, w1), ("по", "поводу") | ("на", "тему") | ("по", "теме") | ("по", "вопросу") | ("на", "счет") | ("в", "отношении") | ("в", "части")) {
            s.drop(i, i + 1);
            i += 2;
            continue;
        }

        // Вопросительные слова и слова о времени, не ставшие периодом: не ищутся, но запоминаются
        // как непонятые — их может довести модель.
        if QUESTION.contains(&w0) || is_date_hint(w0) {
            unknown.push(w0.to_string());
            s.drop(i, i);
            i += 1;
            continue;
        }
        if GLUE.contains(&w0) || STOP.contains(&w0) {
            s.drop(i, i);
            i += 1;
            continue;
        }
        pending.push(i);
        i += 1;
    }

    // Слова поиска и их формы.
    for &idx in &pending {
        let text = fts_norm(s.tokens[idx].value.trim());
        if query::plain_words(&text).next().is_none() {
            s.slots[idx] = Slot::Drop;
            continue;
        }
        for x in query::plain_words(&text) {
            add_forms(&mut base, x, ctx.people);
        }
        base.terms.push(Term { text, phrase: false });
    }
    if let Some(p) = period {
        let ms = |d: Date| d.to_zoned(ctx.tz.clone()).ok().map(|z| z.timestamp().as_millisecond());
        if let Some(start) = p.start.and_then(ms) {
            base.after_ms = Some(start);
        }
        if let Some(end) = p.end.and_then(ms) {
            base.before_ms = Some(end);
        }
    }

    // Запрос с людьми по адресам и запасной — с их именами словами.
    let mut q = base.clone();
    for f in &s.filters {
        match f.kind {
            ChipKind::To => q.to_any.push(f.emails.clone()),
            ChipKind::With => q.with_any.push(f.emails.clone()),
            _ => q.from_exact.extend(f.emails.iter().cloned()),
        }
    }
    if q.is_empty() {
        // Одни служебные слова («от», «в») — ищем как набрано.
        return Understood::literal(input);
    }
    let mentioned: Vec<&String> = s.filters.iter().flat_map(|f| &f.mention).collect();
    let relaxed = (!mentioned.is_empty()).then(|| {
        let mut r = base.clone();
        for f in s.filters.iter().filter(|f| f.mention.is_empty()) {
            r.from_exact.extend(f.emails.iter().cloned());
        }
        for word in &mentioned {
            let alts: Vec<String> = names::name_words(word).into_iter().filter(|a| a != *word).collect();
            if !alts.is_empty() {
                r.forms.insert((*word).clone(), alts);
            }
            r.terms.push(Term { text: (*word).clone(), phrase: false });
        }
        (r, format!("показаны и письма с упоминанием: {}", mentioned.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(" ")))
    });

    let mut parts: Vec<String> = Vec::new();
    for (idx, slot) in s.slots.iter().enumerate() {
        match slot {
            Slot::Keep => parts.push(s.typed(idx, idx)),
            Slot::Chip(c) => parts.push(s.chips[*c].operator.clone()),
            Slot::Drop | Slot::Cont => {}
        }
    }
    Understood { query: q, chips: s.chips, canonical: parts.join(" "), unknown, relaxed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn people() -> PeopleIndex {
        PeopleIndex::new([
            (Some("Иванов Иван Петрович".to_string()), "ivanov@x.ru".to_string(), 50),
            (Some("Петров Василий Ильич".to_string()), "v.petrov@x.ru".to_string(), 40),
            (Some("Иванова Анна Сергеевна".to_string()), "a.ivanova@x.ru".to_string(), 30),
            (Some("Диспетчерская служба".to_string()), "ods@x.ru".to_string(), 25),
            (Some("Петров Василий Ильич".to_string()), "petrov.old@y.ru".to_string(), 5),
            (Some("Смирнов Василий Олегович".to_string()), "smirnov@x.ru".to_string(), 2),
        ])
    }

    /// Разбор на пятницу 13 ноября 2026, пояс UTC.
    fn u(input: &str) -> Understood {
        let people = people();
        let own = vec!["me@x.ru".to_string()];
        let folders = vec![("Входящие".to_string(), Some("inbox".to_string())), ("Отправленные".to_string(), Some("sentitems".to_string())), ("Проекты 2026".to_string(), None), ("Проекты".to_string(), None)];
        understand(input, &Context { today: date(2026, 11, 13), tz: TimeZone::UTC, people: &people, own: &own, folders: &folders })
    }

    fn ms(y: i16, m: i8, d: i8) -> Option<i64> {
        Some(date(y, m, d).to_zoned(TimeZone::UTC).unwrap().timestamp().as_millisecond())
    }

    fn terms(x: &Understood) -> Vec<&str> {
        x.query.terms.iter().map(|t| t.text.as_str()).collect()
    }

    fn labels(x: &Understood) -> Vec<&str> {
        x.chips.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn sender_topic_and_period() {
        let x = u("письма от диспетчера про ремонт за октябрь");
        assert_eq!(x.query.from_exact, ["ods@x.ru"]);
        assert_eq!(terms(&x), ["ремонт"]);
        assert_eq!((x.query.after_ms, x.query.before_ms), (ms(2026, 10, 1), ms(2026, 11, 1)));
        assert_eq!(labels(&x), ["от: Диспетчерская служба", "период: октябрь 2026"]);
        assert_eq!(x.canonical, "от:ods@x.ru ремонт дата:2026-10");
        assert!(x.unknown.is_empty());
        // Место чипа — в исходном запросе.
        let input: Vec<u16> = "письма от диспетчера про ремонт за октябрь".encode_utf16().collect();
        assert_eq!(String::from_utf16(&input[x.chips[0].start..x.chips[0].end]).unwrap(), "от диспетчера");
        assert_eq!(String::from_utf16(&input[x.chips[1].start..x.chips[1].end]).unwrap(), "за октябрь");
        // Период можно искать и словом.
        assert_eq!(x.chips[1].options, [ChipOption { label: "искать как слова".into(), operator: "\"октябрь\"".into() }]);
        // Запасной запрос: без адреса, имя словом.
        let (relaxed, note) = x.relaxed.unwrap();
        assert!(relaxed.from_exact.is_empty());
        assert_eq!(relaxed.terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["ремонт", "диспетчера"]);
        assert_eq!(relaxed.forms["диспетчера"], ["диспетчер"]);
        assert_eq!(note, "показаны и письма с упоминанием: диспетчера");
    }

    #[test]
    fn verbs_and_first_names() {
        let x = u("что писал Вася про запчасти на Колыму в прошлом месяце");
        // Одно личное имя — самый частый Василий, остальные — вариантами.
        assert_eq!(x.query.from_exact, ["v.petrov@x.ru", "petrov.old@y.ru"]);
        assert_eq!(terms(&x), ["запчасти", "Колыму"]);
        assert_eq!((x.query.after_ms, x.query.before_ms), (ms(2026, 10, 1), ms(2026, 11, 1)));
        assert_eq!(labels(&x), ["от: Петров Василий Ильич", "период: октябрь 2026"]);
        assert_eq!(x.chips[0].note.as_deref(), Some("ещё 1 с таким именем"));
        assert_eq!(x.chips[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Смирнов Василий Олегович", "все с таким именем (2)", "искать как слова"]);
        assert_eq!(x.chips[0].options[0].operator, "от:smirnov@x.ru");
        assert_eq!(x.chips[0].options[1].operator, "от:v.petrov@x.ru,petrov.old@y.ru,smirnov@x.ru");
        assert_eq!(x.canonical, "от:v.petrov@x.ru,petrov.old@y.ru запчасти Колыму дата:2026-10");
        assert!(x.query.forms.contains_key("запчасти") && x.query.forms.contains_key("колыму"));

        // Имя перед глаголом.
        let x = u("Иванова прислала счет вчера");
        assert_eq!(x.query.from_exact, ["ivanov@x.ru", "a.ivanova@x.ru"], "родительный мужской и именительный женской фамилии");
        assert_eq!(labels(&x), ["от: Иванов Иван Петрович и ещё 1", "период: 12 ноября 2026"]);
        assert_eq!(terms(&x), ["счет"]);
        assert_eq!(x.canonical, "от:ivanov@x.ru,a.ivanova@x.ru счет дата:2026-11-12");
        // Глагол без человека — просто служебное слово.
        let x = u("что писали про ремонт");
        assert!(x.chips.is_empty());
        assert_eq!(terms(&x), ["ремонт"]);
    }

    #[test]
    fn recipients_and_correspondence() {
        let x = u("я писал Петрову про договор");
        assert_eq!(x.query.from_exact, ["me@x.ru"]);
        assert_eq!(x.query.to_any, [vec!["v.petrov@x.ru".to_string(), "petrov.old@y.ru".to_string()]]);
        assert_eq!(terms(&x), ["договор"]);
        assert_eq!(labels(&x), ["от: меня", "кому: Петров Василий Ильич"]);
        assert_eq!(x.canonical, "от:me@x.ru кому:v.petrov@x.ru,petrov.old@y.ru договор");
        // В запасном запросе «от меня» остаётся.
        assert_eq!(x.relaxed.unwrap().0.from_exact, ["me@x.ru"]);

        let x = u("переписка с Ивановым Иваном за 2025 год");
        assert_eq!(x.query.with_any, [vec!["ivanov@x.ru".to_string()]]);
        assert_eq!(labels(&x), ["переписка с: Иванов Иван Петрович", "период: 2025 год"]);
        assert_eq!(x.canonical, "переписка:ivanov@x.ru дата:2025");
        assert!(x.query.terms.is_empty());

        let x = u("мои письма Ивановой");
        assert_eq!(x.query.to_any, [vec!["a.ivanova@x.ru".to_string()]]);
        let x = u("от меня за сегодня");
        assert_eq!(x.query.from_exact, ["me@x.ru"]);
        assert_eq!(x.query.after_ms, ms(2026, 11, 13));
    }

    #[test]
    fn attachments_folders_and_state() {
        let x = u("найди все письма от Ивановой с вложением за прошлую неделю");
        assert_eq!(x.query.from_exact, ["a.ivanova@x.ru"]);
        assert!(x.query.has_attachment && x.query.attachment_kinds.is_empty());
        assert_eq!((x.query.after_ms, x.query.before_ms), (ms(2026, 11, 2), ms(2026, 11, 9)));
        assert_eq!(labels(&x), ["от: Иванова Анна Сергеевна", "с вложениями", "период: 2–8 ноября 2026"]);
        assert_eq!(x.canonical, "от:a.ivanova@x.ru вложение:да после:2026-11-02 до:2026-11-08");
        assert!(x.query.terms.is_empty());

        let x = u("непрочитанные во входящих с pdf");
        assert_eq!((x.query.unread, x.query.folder.as_deref(), x.query.has_attachment), (Some(true), Some("inbox"), true));
        assert_eq!(x.query.attachment_kinds, [AttachmentKind::Pdf]);
        assert_eq!(x.canonical, "прочитано:нет папка:inbox вложение:pdf");
        assert_eq!(labels(&x), ["непрочитанные", "папка: Входящие", "вложение: PDF"]);

        let x = u("счета с таблицей в отправленных с флагом");
        assert_eq!(x.query.attachment_kinds, [AttachmentKind::Sheet]);
        assert_eq!((x.query.folder.as_deref(), x.query.flagged), (Some("sentitems"), Some(true)));
        assert_eq!(terms(&x), ["счета"]);

        // Папка по имени: имя из стольких же слов — точнее.
        let x = u("акт в папке проекты 2026");
        assert_eq!(x.query.folder.as_deref(), Some("Проекты 2026"));
        assert_eq!(x.canonical, "акт папка:\"Проекты 2026\"");
        let x = u("акт в папке проектов");
        assert_eq!(x.query.folder.as_deref(), Some("Проекты"));
        // Нет такой папки — слова остаются словами.
        let x = u("в папке несуществующей акт");
        assert!(x.query.folder.is_none());
        assert_eq!(terms(&x), ["папке", "несуществующей", "акт"]);

        let x = u("в теме договор поставки за октябрь");
        assert_eq!(x.query.subject, ["договор поставки"]);
        assert_eq!(x.canonical, "тема:\"договор поставки\" дата:2026-10");
        assert!(x.query.forms.contains_key("поставки"));
    }

    #[test]
    fn plain_words_get_forms_and_noise_is_dropped() {
        let x = u("счет на оплату");
        assert!(x.chips.is_empty());
        assert_eq!(terms(&x), ["счет", "оплату"]);
        assert_eq!(x.canonical, "счет оплату");
        assert!(x.query.forms["оплату"].iter().any(|f| f == "оплата") && !x.query.forms.contains_key("счет"));
        assert_eq!(x.query.fts_match().unwrap().split(" AND ").count(), 2);

        // Фамилия из ящика в начальной форме не разбирается, в косвенном падеже — разбирается.
        assert!(u("иванов").query.forms.is_empty());
        assert!(u("иванову").query.forms["иванову"].iter().any(|f| f == "иванов"));
        // К имени фамилия не добавляется.
        let x = u("ивана");
        assert!(x.query.forms["ивана"].iter().any(|f| f == "иван") && !x.query.forms["ивана"].iter().any(|f| f == "иванов"));
        // Слово не из ящика — все формы.
        assert!(u("договора").query.forms["договора"].iter().any(|f| f == "договоров"));

        // Операторы и фразы — как набраны.
        let x = u("от:ivanov@x.ru вчера \"точная фраза\" тема:счет");
        assert_eq!(x.query.from_exact, ["ivanov@x.ru"]);
        assert_eq!(x.query.terms, [Term { text: "точная фраза".into(), phrase: true }]);
        assert_eq!(x.query.subject, ["счет"]);
        assert_eq!(x.canonical, "от:ivanov@x.ru дата:2026-11-12 \"точная фраза\" тема:счет");
        assert!(x.relaxed.is_none(), "человек задан оператором — ослаблять нечего");
        // Слово в кавычках не становится периодом.
        let x = u("отчет \"октябрь\"");
        assert!(x.chips.is_empty() && x.query.after_ms.is_none());
    }

    #[test]
    fn composes_model_fields_into_operators() {
        let people = people();
        let own = vec!["Me@x.ru".to_string()];
        let folders = vec![("Входящие".to_string(), Some("inbox".to_string()))];
        let ctx = Context { today: date(2026, 11, 13), tz: TimeZone::UTC, people: &people, own: &own, folders: &folders };
        let f = ModelFields {
            from: vec!["Диспетчер".into(), "Поставщик Ромашка".into(), "Boss@Y.ru".into()],
            to: vec!["Вася".into()],
            with: vec!["Иванов".into()],
            from_me: true,
            period: Some(Period { start: Some(date(2026, 10, 1)), end: Some(date(2026, 11, 1)) }),
            has_attachment: true,
            attachment_kind: Some(AttachmentKind::Sheet),
            folder: Some("inbox".into()),
            unread: true,
            flagged: true,
            words: vec!["ремонт".into(), "тема:насос".into(), "\"".into()],
            phrases: vec!["№ 15/2026".into()],
        };
        let c = compose(&f, &ctx);
        assert_eq!(
            c.operators,
            "от:me@x.ru от:ods@x.ru от:\"Поставщик Ромашка\" от:boss@y.ru кому:v.petrov@x.ru,petrov.old@y.ru переписка:ivanov@x.ru дата:2026-10 вложение:таблица папка:inbox прочитано:нет флаг:да ремонт тема насос \"№ 15/2026\""
        );
        assert_eq!(
            c.labels,
            [
                "от: меня",
                "от: Диспетчерская служба",
                "от: Поставщик Ромашка",
                "от: Boss@Y.ru",
                "кому: Петров Василий Ильич",
                "переписка с: Иванов Иван Петрович",
                "период: октябрь 2026",
                "вложение: таблица",
                "папка: Входящие",
                "непрочитанные",
                "с флагом",
                "слова: ремонт тема насос",
                "фраза: «№ 15/2026»"
            ]
        );
        // Собранный запрос разбирается теми же правилами, что и набранный руками.
        let q = understand(&c.operators, &ctx).query;
        assert_eq!(q.from_exact, ["me@x.ru", "ods@x.ru", "boss@y.ru"]);
        assert_eq!(q.from, ["Поставщик Ромашка"]);
        assert_eq!(q.to_any, [vec!["v.petrov@x.ru".to_string(), "petrov.old@y.ru".to_string()]]);
        assert_eq!(q.with_any, [vec!["ivanov@x.ru".to_string()]], "именительный падеж — только Иванов, без Ивановой");
        assert_eq!((q.unread, q.flagged, q.folder.as_deref()), (Some(true), Some(true), Some("inbox")));
        assert_eq!(q.attachment_kinds, [AttachmentKind::Sheet]);
        assert_eq!(q.terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["№ 15/2026", "ремонт", "тема", "насос"], "фраза и слова; порядок условий на поиск не влияет");
        // Пусто — пустой запрос; без своих адресов «от меня» не появляется.
        assert_eq!(compose(&ModelFields::default(), &ctx), Composed::default());
        let nobody = Context { today: date(2026, 11, 13), tz: TimeZone::UTC, people: &people, own: &[], folders: &[] };
        let c = compose(&ModelFields { from_me: true, has_attachment: true, folder: Some("чужая".into()), ..ModelFields::default() }, &nobody);
        assert_eq!((c.operators.as_str(), c.labels.len()), ("вложение:да", 1));
    }

    #[test]
    fn unknown_tail_and_literal_fallback() {
        // Человека нет среди корреспондентов: слово ищется как слово и запоминается.
        let x = u("от поставщика насосы");
        assert!(x.query.from_exact.is_empty() && x.chips.is_empty());
        assert_eq!(terms(&x), ["поставщика", "насосы"]);
        assert_eq!(x.unknown, ["поставщика"]);
        // Вопрос и время, не ставшее периодом.
        let x = u("когда иванов обещал акт на прошлой планерке");
        assert_eq!(x.unknown, ["когда", "прошлой"]);
        assert_eq!(terms(&x), ["иванов", "обещал", "акт", "планерке"]);
        // Одни служебные слова — как набрано.
        for input in ["от", "и", "в", "найди", "письма"] {
            let x = u(input);
            assert_eq!(terms(&x), [input], "{input}");
            assert!(x.chips.is_empty());
        }
        assert!(u("   ").query.is_empty());
        // Набор на ходу: недописанное слово ищется по началу.
        let x = u("договор с влож");
        assert_eq!(terms(&x), ["договор", "влож"]);
        // Знаки и смайлики не ломают места чипов.
        let x = u("😀 счет за октябрь?");
        let input: Vec<u16> = "😀 счет за октябрь?".encode_utf16().collect();
        assert_eq!(String::from_utf16(&input[x.chips[0].start..x.chips[0].end]).unwrap(), "за октябрь?");
        assert_eq!(terms(&x), ["счет"]);
    }
}
