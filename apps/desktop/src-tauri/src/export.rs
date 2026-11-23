//! Выгрузка вложений агенту (`docs/mcp-attachments.md`, доработка 2). Агент в своей песочнице не
//! видит кэш вложений AIOWA, поэтому по его просьбе вложение копируется в «папку выгрузки» —
//! обычную рабочую папку, которую пользователь сам выбрал в настройках и подключил к агенту.
//!
//! На диск здесь пишется только внутрь этой папки: подпапка — одно имя без пути, настоящий путь
//! каталога проверяется после раскрытия ссылок и точек соединения. Исполняемые файлы и сценарии не
//! выгружаются. Каждая выгрузка записана в базе (`mcp_exports`): по этим записям пользователь
//! видит, что и куда ушло, и по ним же старые файлы удаляются — только те, что выгрузила AIOWA, и
//! только если их с тех пор не меняли.

use aiowa_core::AccountId;
use aiowa_store::Store;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Сколько вложений можно выгрузить за один вызов.
pub(crate) const FILES_PER_CALL: usize = 20;
/// Предел размера выгружаемого файла.
pub(crate) const MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Сколько дней хранятся выгруженные файлы, если пользователь не задал свой срок.
pub(crate) const KEEP_DAYS: u32 = 30;
/// Предел длины имени файла и подпапки, символов.
const NAME_CHARS: usize = 150;
const DAY_MS: i64 = 24 * 3600 * 1000;

/// Отказ выгрузки: неверный запрос или сбой диска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExportError {
    Invalid(String),
    Failed(String),
}

/// Путь для показа и для ответа агенту: без служебного начала `\\?\`, которое даёт канонизация.
pub(crate) fn plain(path: &Path) -> String {
    let s = path.display().to_string();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").map(String::from).unwrap_or(s),
    }
}

/// Имя устройства Windows (`CON`, `NUL`, `COM1`…): файл или папку так назвать нельзя.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit() && c != '0'))
}

fn forbidden(c: char) -> bool {
    c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

/// Имя выгруженного файла: `<номер вложения>-<исходное имя>` без пути и недопустимых в Windows
/// знаков, не длиннее 150 символов; расширение при укорачивании сохраняется.
pub(crate) fn export_name(attachment_pk: i64, name: Option<&str>) -> String {
    let base = name.unwrap_or("").rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base.chars().map(|c| if forbidden(c) { '_' } else { c }).collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let clean = if clean.is_empty() || reserved(clean) { format!("вложение {clean}").trim().to_string() } else { clean.to_string() };
    let full = format!("{attachment_pk}-{clean}");
    let count = full.chars().count();
    if count <= NAME_CHARS {
        return full;
    }
    // Слишком длинное: режется середина, расширение остаётся.
    let extension = full.rsplit_once('.').map(|(_, e)| e).filter(|e| (1..=10).contains(&e.chars().count()) && e.chars().all(char::is_alphanumeric));
    match extension {
        Some(ext) => {
            let keep = NAME_CHARS - ext.chars().count() - 1;
            format!("{}.{ext}", full.chars().take(keep).collect::<String>().trim_end_matches(|c: char| c == '.' || c.is_whitespace()))
        }
        None => full.chars().take(NAME_CHARS).collect(),
    }
}

/// Подпапка внутри папки выгрузки: одно имя, без пути, `..`, буквы диска и недопустимых знаков.
pub(crate) fn check_subfolder(raw: &str) -> Result<String, ExportError> {
    let name = raw.trim();
    let bad = name.is_empty() || name == "." || name == ".." || name.chars().count() > NAME_CHARS || name.chars().any(forbidden) || name.ends_with(['.', ' ']) || reserved(name);
    if bad { Err(ExportError::Invalid("Параметр subfolder — одно имя папки: без пути, «..», буквы диска и знаков < > : \" / \\ | ? *".into())) } else { Ok(name.to_string()) }
}

/// Исполняемый файл или сценарий: такие вложения не выгружаются.
pub(crate) fn blocked(name: Option<&str>) -> bool {
    crate::ondemand::is_unsafe_to_open(name)
}

/// Папка выгрузки из настроек — настоящий (канонический) путь существующего каталога.
pub(crate) fn open_root(configured: &str) -> Result<PathBuf, ExportError> {
    let path = Path::new(configured.trim());
    let real = std::fs::canonicalize(path).ok().filter(|p| p.is_dir());
    real.ok_or_else(|| ExportError::Invalid(format!("Папка выгрузки «{}» не найдена. Пользователь выбирает её в настройках AIOWA: вкладка «ИИ» → «Доступ для агентов (MCP)».", configured.trim())))
}

/// Годится ли каталог, выбранный пользователем, в папки выгрузки: существует, не корень диска и не
/// лежит внутри данных AIOWA (`data_dir` — каталог базы). Возвращает путь в обычном виде.
pub(crate) fn check_root_choice(chosen: &Path, data_dir: &Path) -> Result<String, String> {
    if !chosen.is_absolute() {
        return Err("Нужен полный путь к папке".into());
    }
    let real = std::fs::canonicalize(chosen).ok().filter(|p| p.is_dir()).ok_or_else(|| "Такой папки нет".to_string())?;
    if real.parent().is_none() {
        return Err("Корень диска не подходит: выберите обычную рабочую папку".into());
    }
    if std::fs::canonicalize(data_dir).is_ok_and(|data| real.starts_with(&data)) {
        return Err("Эта папка — внутри данных AIOWA: выберите обычную рабочую папку, например в «Документах»".into());
    }
    Ok(plain(&real))
}

/// Выгруженный файл.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Exported {
    pub name: String,
    /// Полный путь в обычном виде.
    pub path: String,
    pub size: u64,
    /// Файл уже лежал там и не перезаписывался.
    pub already: bool,
}

/// Скопировать файл кэша в папку выгрузки (`root` — её настоящий путь) под именем `name`.
/// Существующий файл без `overwrite` не трогается.
pub(crate) fn export_file(root: &Path, subfolder: Option<&str>, name: &str, cached: &Path, overwrite: bool) -> Result<Exported, ExportError> {
    let disk = |what: &str, e: std::io::Error| ExportError::Failed(format!("{what}: {e}"));
    let dir = match subfolder {
        Some(sub) => root.join(sub),
        None => root.to_path_buf(),
    };
    std::fs::create_dir_all(&dir).map_err(|e| disk("Подпапка не создана", e))?;
    // Граница записи: каталог после раскрытия ссылок и точек соединения — внутри папки выгрузки.
    let real = std::fs::canonicalize(&dir).map_err(|e| disk("Папка выгрузки недоступна", e))?;
    if !real.starts_with(root) {
        return Err(ExportError::Invalid("Подпапка ведёт за пределы папки выгрузки".into()));
    }
    let target = real.join(name);
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() || meta.is_dir() {
            return Err(ExportError::Invalid(format!("На месте файла «{name}» в папке выгрузки — ссылка или папка")));
        }
        if !overwrite {
            return Ok(Exported { name: name.to_string(), path: plain(&target), size: meta.len(), already: true });
        }
    }
    let size = std::fs::metadata(cached).map_err(|e| disk("Файл вложения недоступен", e))?.len();
    if size > MAX_BYTES {
        return Err(ExportError::Invalid(format!("Файл «{name}» больше {} МБ", MAX_BYTES / (1024 * 1024))));
    }
    std::fs::copy(cached, &target).map_err(|e| disk("Файл не скопирован в папку выгрузки", e))?;
    Ok(Exported { name: name.to_string(), path: plain(&target), size, already: false })
}

/// Удалить выгруженные файлы старше срока хранения. Удаляется только записанное в журнале, только
/// внутри нынешней папки выгрузки (`root` — её настоящий путь) и только не изменённое после
/// выгрузки (тот же размер); запись о файле, который изменили, перенесли или которого уже нет,
/// просто убирается. Возвращает число удалённых файлов.
pub(crate) fn sweep(store: &Store, account: AccountId, root: Option<&Path>, keep_days: u32, now_ms: i64) -> usize {
    let old = match store.mcp_exports_before(account, now_ms - i64::from(keep_days.max(1)) * DAY_MS) {
        Ok(old) => old,
        Err(e) => {
            warn!(error = %e, "журнал выгрузок не прочитан, старые файлы не убраны");
            return 0;
        }
    };
    let mut removed = 0;
    for e in old {
        let path = Path::new(&e.path);
        let ours = root.is_some_and(|r| std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(r))) && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() as i64 == e.size);
        if ours && std::fs::remove_file(path).is_ok() {
            removed += 1;
            // Подпапка, если она опустела; сама папка выгрузки остаётся.
            if let Some(parent) = path.parent().filter(|p| root.is_some_and(|r| std::fs::canonicalize(p).is_ok_and(|real| real != r))) {
                let _ = std::fs::remove_dir(parent);
            }
        }
        if let Err(err) = store.mcp_export_forget(e.export_pk) {
            warn!(error = %err, "запись о выгрузке не убрана");
        }
    }
    if removed > 0 {
        info!(removed, keep_days, "выгруженные агенту файлы старше срока хранения удалены");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_store::McpExport;

    fn dirs(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("aiowa-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("out")).unwrap();
        std::fs::create_dir_all(base.join("cache")).unwrap();
        (std::fs::canonicalize(base.join("out")).unwrap(), base.join("cache"))
    }

    #[test]
    fn names_are_made_safe_for_windows() {
        assert_eq!(export_name(512, Some("уведомление.pdf")), "512-уведомление.pdf");
        assert_eq!(export_name(7, Some(r#"C:\чужой\путь\Смета: "итог" <v2>?.xlsx"#)), "7-Смета_ _итог_ _v2__.xlsx");
        assert_eq!(export_name(7, Some("../../etc/passwd")), "7-passwd");
        assert_eq!(export_name(7, Some("  ..  ")), "7-вложение");
        assert_eq!(export_name(7, None), "7-вложение");
        assert_eq!(export_name(7, Some("CON.txt")), "7-вложение CON.txt");
        // Длинное имя: 150 символов, расширение на месте.
        let long = export_name(12345, Some(&format!("{}.docx", "очень длинное название ".repeat(20))));
        assert_eq!(long.chars().count(), 150);
        assert!(long.starts_with("12345-очень длинное") && long.ends_with(".docx"), "{long}");
        assert_eq!(export_name(1, Some(&"я".repeat(300))).chars().count(), 150);
    }

    #[test]
    fn subfolder_is_one_plain_name() {
        assert_eq!(check_subfolder(" Тендер 2026 ").unwrap(), "Тендер 2026");
        for bad in ["", " ", ".", "..", r"..\..\Windows", "a/b", r"a\b", "C:", "C:папка", "что?", "имя.", "NUL", "com1", "lpt9.txt", &"д".repeat(151)] {
            assert!(matches!(check_subfolder(bad), Err(ExportError::Invalid(_))), "{bad}");
        }
        assert!(check_subfolder("COM0").is_ok() && check_subfolder("console").is_ok());
    }

    #[test]
    fn executables_are_not_exported() {
        for name in ["setup.exe", "run.BAT", "a.cmd", "s.ps1", "x.js", "m.vbs", "saver.scr", "ярлык.lnk"] {
            assert!(blocked(Some(name)), "{name}");
        }
        for name in ["скан.pdf", "таблица.xlsx", "фото.jpg", "подпись.sig", "без расширения"] {
            assert!(!blocked(Some(name)), "{name}");
        }
    }

    #[test]
    fn file_is_copied_once_and_stays_inside_the_folder() {
        let (root, cache) = dirs("copy");
        let cached = cache.join("7-смета.pdf");
        std::fs::write(&cached, "первая версия").unwrap();
        let first = export_file(&root, None, "7-смета.pdf", &cached, false).unwrap();
        assert_eq!((first.already, first.size, first.name.as_str()), (false, 13 + 12, "7-смета.pdf"));
        assert!(!first.path.starts_with(r"\\?\") && Path::new(&first.path).is_file(), "{}", first.path);
        // Кэш не тронут; повтор без overwrite ничего не копирует.
        std::fs::write(&cached, b"v2").unwrap();
        let again = export_file(&root, None, "7-смета.pdf", &cached, false).unwrap();
        assert_eq!((again.already, again.size), (true, first.size));
        assert_eq!(std::fs::read(&first.path).unwrap(), "первая версия".as_bytes());
        let over = export_file(&root, None, "7-смета.pdf", &cached, true).unwrap();
        assert_eq!((over.already, over.size, std::fs::read(&over.path).unwrap()), (false, 2, b"v2".to_vec()));
        // Подпапка создаётся внутри; на месте файла папка — отказ.
        let sub = export_file(&root, Some("Тендер"), "7-смета.pdf", &cached, false).unwrap();
        assert!(Path::new(&sub.path).starts_with(plain(&root)) && sub.path.contains("Тендер"));
        std::fs::create_dir_all(root.join("8-папка.pdf")).unwrap();
        assert!(matches!(export_file(&root, None, "8-папка.pdf", &cached, true), Err(ExportError::Invalid(_))));
        assert!(matches!(export_file(&root, None, "9-нет.pdf", &cache.join("нет такого"), false), Err(ExportError::Failed(_))));
    }

    #[test]
    fn link_out_of_the_folder_is_refused() {
        let (root, cache) = dirs("link");
        let outside = cache.join("снаружи");
        std::fs::create_dir_all(&outside).unwrap();
        let cached = cache.join("файл.txt");
        std::fs::write(&cached, b"x").unwrap();
        // Точка соединения внутри папки выгрузки, ведущая наружу (создать её можно без особых прав).
        let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(root.join("лазейка")).arg(&outside).output().is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("точку соединения создать не удалось, тест пропущен");
            return;
        }
        assert_eq!(export_file(&root, Some("лазейка"), "1-файл.txt", &cached, false), Err(ExportError::Invalid("Подпапка ведёт за пределы папки выгрузки".into())));
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "наружу ничего не записано");
    }

    #[test]
    fn folder_choice_is_checked() {
        let (root, cache) = dirs("choice");
        let data = cache.join("AIOWA");
        std::fs::create_dir_all(data.join("attachments")).unwrap();
        assert_eq!(check_root_choice(&root, &data).unwrap(), plain(&root));
        assert!(check_root_choice(Path::new("относительная"), &data).unwrap_err().contains("полный путь"));
        assert!(check_root_choice(&root.join("нет такой"), &data).unwrap_err().contains("нет"));
        assert!(check_root_choice(&data.join("attachments"), &data).unwrap_err().contains("внутри данных"));
        let drive = root.ancestors().last().unwrap();
        assert!(check_root_choice(Path::new(&plain(drive)), &data).unwrap_err().contains("Корень диска"), "{}", plain(drive));
        assert!(matches!(open_root(&plain(&root)), Ok(p) if p == root));
        assert!(matches!(open_root(&plain(&root.join("нет"))), Err(ExportError::Invalid(_))));
    }

    #[test]
    fn only_our_old_untouched_files_are_swept() {
        let (root, cache) = dirs("sweep");
        let store = Store::open(cache.join("mail.db")).unwrap();
        let acc = store.account("h", "u").unwrap();
        let now = 100 * DAY_MS;
        let put = |name: &str, bytes: &[u8], at: i64, recorded_size: i64, dir: &Path| {
            let path = dir.join(name);
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(&path, bytes).unwrap();
            store.mcp_export_add(acc, &McpExport { export_pk: 0, at, attachment_pk: 1, name: name.into(), path: plain(&path), size: recorded_size, client: "c".into() }).unwrap();
            path
        };
        let old = put("1-старый.pdf", b"abc", now - 40 * DAY_MS, 3, &root);
        let in_sub = put("2-в подпапке.pdf", b"abc", now - 40 * DAY_MS, 3, &root.join("Тендер"));
        let edited = put("3-правленый.xlsx", b"abcdef", now - 40 * DAY_MS, 3, &root);
        let fresh = put("4-свежий.pdf", b"abc", now - 5 * DAY_MS, 3, &root);
        let elsewhere = put("5-в другой папке.pdf", b"abc", now - 40 * DAY_MS, 3, &cache.join("прежняя"));
        let foreign = root.join("свой файл пользователя.docx");
        std::fs::write(&foreign, b"abc").unwrap();

        assert_eq!(sweep(&store, acc, Some(&root), 30, now), 2);
        assert!(!old.exists() && !in_sub.exists() && !root.join("Тендер").exists(), "старые удалены, пустая подпапка тоже");
        assert!(edited.exists() && fresh.exists() && elsewhere.exists() && foreign.exists(), "изменённый, свежий, чужой и вне папки — на месте");
        assert_eq!(store.mcp_exports_recent(acc, 10).unwrap().iter().map(|e| e.name.clone()).collect::<Vec<_>>(), ["4-свежий.pdf"], "в журнале осталась только свежая выгрузка");
        assert_eq!(sweep(&store, acc, Some(&root), 30, now), 0);
        assert!(root.is_dir());
    }
}
