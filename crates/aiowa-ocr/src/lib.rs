//! Распознавание текста на сканах: страницы PDF без текстового слоя и картинки (фотографии
//! документов). Всё делается на этом компьютере встроенными средствами Windows —
//! `Windows.Data.Pdf` рисует страницу, `Windows.Media.Ocr` читает её; наружу ничего не уходит и
//! сторонние библиотеки не нужны. Вне Windows распознавание недоступно (`OcrError::Unavailable`).
//!
//! Движок Windows отдаёт текст строками и таблицу читает по столбцам; порядок чтения
//! восстанавливается здесь по рамкам строк (`reading_order`): строки одной высоты на странице —
//! одна строка текста, ячейки через табуляцию.
//!
//! Движок читает на одном языке. Страницу читает русский, а латинские слова на ней (адреса почты,
//! марки, английский текст) он путает с похожими русскими буквами, поэтому их уточняет второй
//! проход английским движком — по рамкам слов (`refine_latin`).

#[cfg(windows)]
mod win;

/// Разрешение, в котором рисуется страница PDF перед распознаванием, точек на дюйм.
pub const RENDER_DPI: u32 = 300;
/// Длинная сторона картинки для распознавания, пикселей: страница A3 в 300 dpi помещается, большее
/// уменьшается (движок Windows принимает до 10 000, но время и память растут с площадью).
pub const MAX_SIDE: u32 = 5000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OcrError {
    /// Распознавания нет вообще: не Windows или в системе не установлен ни один язык распознавания.
    #[error("распознавание текста недоступно: {0}")]
    Unavailable(String),
    /// Этот файл или страницу распознать не удалось.
    #[error("{0}")]
    Failed(String),
}

/// Что распознаётся: PDF (страницы рисуются) или картинка (кадры многостраничного TIFF — страницы).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Pdf,
    Image,
}

/// Вид файла по имени и MIME-типу; `None` — такие файлы не распознаются.
pub fn source_kind(name: Option<&str>, content_type: Option<&str>) -> Option<SourceKind> {
    let ext = name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.trim().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => return Some(SourceKind::Pdf),
        "jpg" | "jpeg" | "jpe" | "png" | "tif" | "tiff" | "bmp" => return Some(SourceKind::Image),
        _ => {}
    }
    let ct = content_type.map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase()).unwrap_or_default();
    match ct.as_str() {
        "application/pdf" => Some(SourceKind::Pdf),
        "image/jpeg" | "image/pjpeg" | "image/png" | "image/tiff" | "image/bmp" => Some(SourceKind::Image),
        _ => None,
    }
}

/// Строка, как её увидел движок: текст и где она лежит на странице (пиксели картинки).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    /// Левый и правый край строки.
    pub left: f32,
    pub right: f32,
    /// Центр первого и последнего слова: по ним виден наклон скана.
    pub first: (f32, f32),
    pub last: (f32, f32),
    /// Высота самого высокого слова.
    pub height: f32,
}

/// Слово, как его увидел движок: текст и рамка на странице (пиксели картинки).
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Сколько в слове латинских и сколько кириллических букв.
fn letters(s: &str) -> (usize, usize) {
    s.chars().fold((0, 0), |(latin, cyrillic), c| {
        if c.is_ascii_alphabetic() {
            (latin + 1, cyrillic)
        } else if ('\u{0400}'..='\u{04FF}').contains(&c) {
            (latin, cyrillic + 1)
        } else {
            (latin, cyrillic)
        }
    })
}

/// Одинаковые на вид буквы: латинская и кириллическая.
const LOOKALIKES: &[(char, char)] = &[
    ('A', 'А'), ('B', 'В'), ('C', 'С'), ('E', 'Е'), ('H', 'Н'), ('K', 'К'), ('M', 'М'), ('O', 'О'), ('P', 'Р'), ('T', 'Т'), ('X', 'Х'),
    ('a', 'а'), ('c', 'с'), ('e', 'е'), ('o', 'о'), ('p', 'р'), ('x', 'х'), ('y', 'у'),
];

/// Русские буквы, которые движок ставит на место латинских: `page` → «раде», `on` → «оп».
const LATIN_SHAPED: &str = "авдеикмнопрстухАВЕИКМНОРСТХ";

/// Какая доля рамки `inner` лежит внутри рамки `outer`.
fn inside(inner: &Word, outer: &Word) -> f32 {
    let dx = (inner.x + inner.w).min(outer.x + outer.w) - inner.x.max(outer.x);
    let dy = (inner.y + inner.h).min(outer.y + outer.h) - inner.y.max(outer.y);
    if dx <= 0.0 || dy <= 0.0 || inner.w <= 0.0 || inner.h <= 0.0 { 0.0 } else { dx * dy / (inner.w * inner.h) }
}

/// То же место страницы, прочитанное английским движком: его слова, лежащие в рамке `word`
/// (движки режут слова по-разному). `None` — там ничего внятного не нашлось.
fn english_for(word: &Word, english: &[Word]) -> Option<String> {
    let mut found: Vec<&Word> = english.iter().filter(|e| inside(e, word) >= 0.6).collect();
    found.sort_by(|a, b| a.x.total_cmp(&b.x));
    let covered: f32 = found.iter().map(|e| ((e.x + e.w).min(word.x + word.w) - e.x.max(word.x)).max(0.0)).sum();
    if found.is_empty() || covered < 0.7 * word.w {
        return None;
    }
    let mut out = String::new();
    for (i, e) in found.iter().enumerate() {
        if i > 0 && e.x - (found[i - 1].x + found[i - 1].w) > 0.3 * word.h {
            out.push(' ');
        }
        out.push_str(e.text.trim());
    }
    (letters(&out).0 > 0).then_some(out)
}

/// Слова строки, прочитанной русским движком, с латиницей от английского (`english` — все слова
/// страницы, как их прочёл он). Заменяются слова, которые русский движок сам счёл латинскими, а в
/// строке сплошь латинской — и остальные (там он путает буквы: `page` → «раде»). В русском слове
/// случайная латинская буква-двойник заменяется русской.
pub fn refine_latin(line: &[Word], english: &[Word]) -> Vec<String> {
    let counted: Vec<(usize, usize)> = line.iter().map(|w| letters(&w.text)).collect();
    let wordy: Vec<&(usize, usize)> = counted.iter().filter(|(latin, cyrillic)| latin + cyrillic >= 2).collect();
    // Строка латиницей: латинских слов не меньше четырёх пятых — остальное в ней, скорее всего, ошибки чтения.
    let latin_line = wordy.len() >= 3 && wordy.iter().filter(|(latin, cyrillic)| latin >= cyrillic).count() * 5 >= wordy.len() * 4;
    line.iter()
        .zip(&counted)
        .map(|(word, (latin, cyrillic))| {
            if latin + cyrillic == 0 {
                return word.text.clone();
            }
            // В латинской строке — ещё и короткое слово из русских букв, похожих на латинские.
            if (*latin > 0 && latin >= cyrillic) || (latin_line && latin + cyrillic <= 4 && word.text.chars().all(|c| !c.is_alphabetic() || c.is_ascii() || LATIN_SHAPED.contains(c))) {
                return english_for(word, english).unwrap_or_else(|| word.text.clone());
            }
            if *latin > 0 {
                return word.text.chars().map(|c| LOOKALIKES.iter().find(|(l, _)| *l == c).map_or(c, |(_, r)| *r)).collect();
            }
            word.text.clone()
        })
        .collect()
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

/// Текст страницы в порядке чтения. Движок отдаёт таблицу столбцами (сначала все названия, потом
/// все числа), а строки наклонённого скана уезжают по вертикали; здесь наклон оценивается по
/// длинным строкам, строки одной высоты на странице собираются в одну слева направо (ячейки —
/// через табуляцию), и всё идёт сверху вниз.
pub fn reading_order(lines: &[Line]) -> String {
    let lines: Vec<&Line> = lines.iter().filter(|l| !l.text.trim().is_empty()).collect();
    let Some(height) = median(lines.iter().map(|l| l.height).collect()).filter(|h| *h > 0.0) else { return String::new() };
    // Наклон: по строкам, которые заметно длиннее своей высоты.
    let slope = median(lines.iter().filter(|l| l.last.0 - l.first.0 > 8.0 * height).map(|l| (l.last.1 - l.first.1) / (l.last.0 - l.first.0)).collect()).unwrap_or(0.0);
    // Высота строки на странице без наклона: где она пересекла бы левый край.
    let mut placed: Vec<(f32, &Line)> = lines.iter().map(|l| (l.first.1 - slope * l.first.0, *l)).collect();
    placed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut rows: Vec<Vec<(f32, &Line)>> = Vec::new();
    for item in placed {
        match rows.last_mut() {
            Some(row) if item.0 - row.last().map_or(item.0, |l| l.0) <= 0.5 * height => row.push(item),
            _ => rows.push(vec![item]),
        }
    }
    let mut out = String::new();
    for mut row in rows {
        row.sort_by(|a, b| a.1.left.total_cmp(&b.1.left));
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&row.iter().map(|(_, l)| l.text.trim()).collect::<Vec<_>>().join("\t"));
    }
    out
}

/// Файл, открытый для распознавания. Страницы нумеруются с единицы.
pub struct Document {
    #[cfg(windows)]
    inner: win::Document,
}

impl Document {
    /// Открыть файл из памяти. `Unavailable` — распознавать на этом компьютере нечем.
    pub fn open(kind: SourceKind, bytes: &[u8]) -> Result<Self, OcrError> {
        #[cfg(windows)]
        {
            win::Document::open(kind, bytes).map(|inner| Self { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = (kind, bytes);
            Err(OcrError::Unavailable("только в Windows".into()))
        }
    }

    pub fn page_count(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.page_count()
        }
        #[cfg(not(windows))]
        {
            0
        }
    }

    /// Язык, на котором читает движок (`ru`, `en-US`…).
    pub fn language(&self) -> &str {
        #[cfg(windows)]
        {
            self.inner.language()
        }
        #[cfg(not(windows))]
        {
            ""
        }
    }

    /// Текст страницы `page` (с единицы) в порядке чтения; пустая строка — на странице текста нет.
    pub fn recognize(&self, page: u32) -> Result<String, OcrError> {
        #[cfg(windows)]
        {
            self.inner.recognize(page)
        }
        #[cfg(not(windows))]
        {
            let _ = page;
            Err(OcrError::Unavailable("только в Windows".into()))
        }
    }
}

/// Язык, на котором будет читать распознавание (`ru`, `en-US`…); `Unavailable` — читать нечем.
pub fn language() -> Result<String, OcrError> {
    #[cfg(windows)]
    {
        win::language()
    }
    #[cfg(not(windows))]
    {
        Err(OcrError::Unavailable("только в Windows".into()))
    }
}

/// Число страниц PDF — и без распознавания, когда нужно только оно.
pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, OcrError> {
    #[cfg(windows)]
    {
        win::pdf_page_count(bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err(OcrError::Unavailable("только в Windows".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, left: f32, right: f32, y_first: f32, y_last: f32) -> Line {
        Line { text: text.into(), left, right, first: (left + 20.0, y_first), last: (right - 20.0, y_last), height: 46.0 }
    }

    #[test]
    fn kinds_by_name_and_type() {
        assert_eq!(source_kind(Some("Скан.PDF"), None), Some(SourceKind::Pdf));
        assert_eq!(source_kind(Some("фото.JPeg"), Some("application/octet-stream")), Some(SourceKind::Image));
        assert_eq!(source_kind(Some("без расширения"), Some("image/tiff; x=1")), Some(SourceKind::Image));
        assert_eq!(source_kind(Some("таблица.xlsx"), Some("application/vnd.ms-excel")), None);
        assert_eq!(source_kind(None, None), None);
    }

    #[test]
    fn table_columns_become_rows() {
        // Так движок отдаёт таблицу: сначала столбец названий, потом столбцы чисел.
        let lines = [
            line("Показатель", 210.0, 500.0, 1500.0, 1500.0),
            line("Мощность, кВт", 210.0, 560.0, 1574.0, 1574.0),
            line("Расход топлива, л/ч", 210.0, 700.0, 1648.0, 1648.0),
            line("Заголовок письма", 210.0, 900.0, 300.0, 300.0),
            line("Норма", 760.0, 900.0, 1500.0, 1500.0),
            line("1600", 800.0, 900.0, 1574.0, 1574.0),
            line("392", 820.0, 900.0, 1648.0, 1648.0),
            line("Факт", 1000.0, 1100.0, 1502.0, 1502.0),
            line("1584", 1000.0, 1100.0, 1576.0, 1576.0),
            line("401", 1010.0, 1100.0, 1650.0, 1650.0),
        ];
        assert_eq!(reading_order(&lines), "Заголовок письма\nПоказатель\tНорма\tФакт\nМощность, кВт\t1600\t1584\nРасход топлива, л/ч\t392\t401");
    }

    #[test]
    fn skewed_scan_keeps_rows_apart_and_together() {
        // Скан наклонён на градус: правый конец длинной строки на 35 пикселей ниже левого, шаг строк 74.
        let k = 0.0175_f32;
        let at = |x: f32, base: f32| base + k * x;
        let long = |text: &str, base: f32| Line { text: text.into(), left: 200.0, right: 2200.0, first: (220.0, at(220.0, base)), last: (2180.0, at(2180.0, base)), height: 46.0 };
        let cell = |text: &str, x: f32, base: f32| Line { text: text.into(), left: x, right: x + 150.0, first: (x + 20.0, at(x + 20.0, base)), last: (x + 130.0, at(x + 130.0, base)), height: 46.0 };
        let lines = [long("Первая длинная строка письма", 400.0), cell("401", 1900.0, 548.0), long("Вторая длинная строка письма", 474.0), cell("Расход", 200.0, 548.0), cell("392", 1200.0, 548.0)];
        assert_eq!(reading_order(&lines), "Первая длинная строка письма\nВторая длинная строка письма\nРасход\t392\t401");
    }

    fn words(texts: &[&str]) -> Vec<Word> {
        // Слова в строку: ширина по числу букв, промежуток 20.
        let mut x = 100.0;
        texts
            .iter()
            .map(|t| {
                let w = 22.0 * t.chars().count() as f32;
                let word = Word { text: (*t).into(), x, y: 500.0, w, h: 46.0 };
                x += w + 20.0;
                word
            })
            .collect()
    }

    #[test]
    fn latin_words_come_from_the_english_pass() {
        // Английская строка: русский движок спутал буквы в двух словах.
        let line = words(&["This", "first", "раде", "has", "а", "real", "text", "layer"]);
        let english = words(&["This", "first", "page", "has", "a", "real", "text", "layer"]);
        assert_eq!(refine_latin(&line, &english).join(" "), "This first page has a real text layer");

        // Русская строка с маркой и адресом: меняются только латинские слова.
        let line = words(&["двигателя", "Cummins", "kTA50-G3;", "e-mail:", "offre@northwind.ru"]);
        let english = words(&["ABMrareJ1R", "Cummins", "KTA50-G3;", "e-mail:", "office@northwind.ru"]);
        assert_eq!(refine_latin(&line, &english).join(" "), "двигателя Cummins KTA50-G3; e-mail: office@northwind.ru");

        // Числа и знаки не трогаются; без английского прохода строка остаётся как есть.
        let line = words(&["Мощность,", "кВт", "1600", "-1,0", "%"]);
        assert_eq!(refine_latin(&line, &words(&["MOUHOCTb,", "KBT", "1600", "-1,0", "%"])).join(" "), "Мощность, кВт 1600 -1,0 %");
        let line = words(&["Cummins", "двигатель"]);
        assert_eq!(refine_latin(&line, &[]).join(" "), "Cummins двигатель");
    }

    #[test]
    fn stray_lookalike_letters_and_split_words() {
        // В русском слове латинская «C» — двойник: становится русской, английское чтение не берётся.
        let line = words(&["Cоглашение", "о", "поставке"]);
        let english = words(&["Cornawenne", "o", "nocTaBKe"]);
        let fixed = refine_latin(&line, &english);
        assert_eq!(fixed.join(" "), "Соглашение о поставке");
        assert!(fixed[0].chars().all(|c| !c.is_ascii()), "все буквы русские");

        // Английский движок разрезал адрес на части: они склеиваются по рамке русского слова.
        let ru = Word { text: "ofce@nопhwind.ru".into(), x: 100.0, y: 500.0, w: 400.0, h: 46.0 };
        let en = [Word { text: "office@northwind".into(), x: 100.0, y: 500.0, w: 340.0, h: 46.0 }, Word { text: ".ru".into(), x: 442.0, y: 500.0, w: 58.0, h: 46.0 }, Word { text: "далеко".into(), x: 900.0, y: 500.0, w: 100.0, h: 46.0 }];
        assert_eq!(refine_latin(std::slice::from_ref(&ru), &en), ["office@northwind.ru"]);
        // Английское слово накрыло только край — не замена.
        assert_eq!(refine_latin(std::slice::from_ref(&ru), &en[1..]), ["ofce@nопhwind.ru"]);
    }

    #[test]
    fn empty_page_gives_empty_text() {
        assert_eq!(reading_order(&[]), "");
        assert_eq!(reading_order(&[line("  ", 0.0, 10.0, 5.0, 5.0)]), "");
    }

    /// PDF в одну или несколько страниц с крупной надписью встроенным шрифтом: страница рисуется и
    /// читается теми же средствами, что и скан.
    fn text_pdf(pages: &[&str]) -> Vec<u8> {
        let mut objs: Vec<Vec<u8>> = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()];
        let kids: String = (0..pages.len()).map(|i| format!("{} 0 R ", 4 + i * 2)).collect();
        objs.push(format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes());
        objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
        for (i, text) in pages.iter().enumerate() {
            objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + i * 2).into_bytes());
            let content = format!("BT /F1 40 Tf 60 700 Td ({text}) Tj ET");
            objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).into_bytes());
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    #[test]
    fn pdf_pages_are_rendered_and_read() {
        let pdf = text_pdf(&["HELLO WORLD 2025", "SECOND PAGE 77"]);
        let doc = match Document::open(SourceKind::Pdf, &pdf) {
            Ok(doc) => doc,
            // На машине без распознавания (сборочный сервер, не Windows) проверять нечего.
            Err(OcrError::Unavailable(why)) => {
                eprintln!("распознавание недоступно, тест пропущен: {why}");
                return;
            }
            Err(e) => panic!("PDF не открылся: {e}"),
        };
        assert_eq!(doc.page_count(), 2);
        assert_eq!(pdf_page_count(&pdf).unwrap(), 2);
        assert!(!doc.language().is_empty());
        let first = doc.recognize(1).unwrap().to_uppercase();
        assert!(first.contains("HELLO") && first.contains("2025"), "{first}");
        let second = doc.recognize(2).unwrap().to_uppercase();
        assert!(second.contains("SECOND") && !second.contains("HELLO"), "{second}");
        assert!(matches!(doc.recognize(3), Err(OcrError::Failed(_))), "страницы 3 нет");
        assert!(matches!(doc.recognize(0), Err(OcrError::Failed(_))));
    }

    #[test]
    fn garbage_is_not_a_document() {
        match Document::open(SourceKind::Pdf, b"this is not a pdf") {
            Err(OcrError::Failed(_)) | Err(OcrError::Unavailable(_)) => {}
            other => panic!("ждали отказ, получили {:?}", other.map(|d| d.page_count())),
        }
    }
}
