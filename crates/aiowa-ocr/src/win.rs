//! Реализация на средствах Windows: `Windows.Data.Pdf` рисует страницу PDF в картинку,
//! `Windows.Graphics.Imaging` читает картинки (в том числе многостраничный TIFF) и приводит их к
//! виду, который принимает распознавание, `Windows.Media.Ocr` читает текст. Все вызовы ждут
//! результата на своём потоке (`join`), поэтому звать их нужно из блокирующего потока.

use crate::{reading_order, refine_latin, Line, OcrError, SourceKind, Word, MAX_SIDE, RENDER_DPI};
use windows::core::{Error, HSTRING};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapFrame, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode, SoftwareBitmap};
use windows::Media::Ocr::{OcrEngine, OcrResult};
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

type WinResult<T> = windows::core::Result<T>;

fn failed(what: &str, e: Error) -> OcrError {
    OcrError::Failed(format!("{what}: {}", e.message()))
}

/// Поток в памяти с содержимым файла: так не нужны пути и права доступа к каталогам.
fn stream_of(bytes: &[u8]) -> WinResult<InMemoryRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes)?;
    writer.StoreAsync()?.join()?;
    writer.FlushAsync()?.join()?;
    // Поток остаётся нашим: без этого писатель закрыл бы его вместе с собой.
    writer.DetachStream()?;
    stream.Seek(0)?;
    Ok(stream)
}

/// Движки распознавания. Основной — русский, если он установлен, иначе язык пользователя, иначе
/// любой установленный. Второй — английский, если основной не он: им уточняются латинские слова.
struct Engines {
    main: OcrEngine,
    english: Option<OcrEngine>,
    /// Языки через плюс: `ru+en-US`.
    language: String,
}

fn engines() -> Result<Engines, OcrError> {
    let (main, tag) = engine()?;
    let english = if tag.to_ascii_lowercase().starts_with("en") { None } else { OcrEngine::AvailableRecognizerLanguages().ok().and_then(|all| all.into_iter().find(|l| l.LanguageTag().is_ok_and(|t| t.to_string().to_ascii_lowercase().starts_with("en")))).and_then(|l| OcrEngine::TryCreateFromLanguage(&l).ok()) };
    let second = english.as_ref().and_then(|e| e.RecognizerLanguage().and_then(|l| l.LanguageTag()).ok()).map(|t| format!("+{t}")).unwrap_or_default();
    Ok(Engines { main, english, language: format!("{tag}{second}") })
}

/// Слова результата по строкам, с рамками.
fn words_of(result: &OcrResult) -> WinResult<Vec<Vec<Word>>> {
    let mut lines = Vec::new();
    for line in result.Lines()? {
        let words: Vec<Word> = line.Words()?.into_iter().filter_map(|w| Some((w.Text().ok()?.to_string(), w.BoundingRect().ok()?))).map(|(text, r)| Word { text, x: r.X, y: r.Y, w: r.Width, h: r.Height }).collect();
        if !words.is_empty() {
            lines.push(words);
        }
    }
    Ok(lines)
}

/// Основной движок: русский, если он установлен (он читает и латиницу), иначе язык
/// пользователя, иначе любой установленный.
fn engine() -> Result<(OcrEngine, String), OcrError> {
    let russian = Language::CreateLanguage(&HSTRING::from("ru")).ok().filter(|l| OcrEngine::IsLanguageSupported(l).unwrap_or(false)).and_then(|l| OcrEngine::TryCreateFromLanguage(&l).ok());
    let engine = russian
        .or_else(|| OcrEngine::TryCreateFromUserProfileLanguages().ok())
        .or_else(|| OcrEngine::AvailableRecognizerLanguages().ok().and_then(|all| all.into_iter().next()).and_then(|l| OcrEngine::TryCreateFromLanguage(&l).ok()))
        .ok_or_else(|| OcrError::Unavailable("в Windows не установлено распознавание текста ни для одного языка (Параметры → Время и язык → Язык и регион → языковые компоненты)".into()))?;
    let language = engine.RecognizerLanguage().and_then(|l| l.LanguageTag()).map(|t| t.to_string()).unwrap_or_default();
    Ok((engine, language))
}

enum Source {
    Pdf(PdfDocument),
    Image(BitmapDecoder),
}

pub(crate) struct Document {
    source: Source,
    engines: Engines,
    pages: u32,
}

/// Кадр картинки в виде, который принимает распознавание: BGRA, длинная сторона не больше
/// `MAX_SIDE`; у фотографий учитывается поворот из EXIF.
fn bitmap(frame: &BitmapFrame, exif: ExifOrientationMode) -> WinResult<SoftwareBitmap> {
    let (width, height) = (frame.PixelWidth()?.max(1), frame.PixelHeight()?.max(1));
    let k = (f64::from(MAX_SIDE) / f64::from(width.max(height))).min(1.0);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(((f64::from(width) * k).round() as u32).max(1))?;
    transform.SetScaledHeight(((f64::from(height) * k).round() as u32).max(1))?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    frame.GetSoftwareBitmapTransformedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied, &transform, exif, ColorManagementMode::DoNotColorManage)?.join()
}

/// Страница PDF картинкой в `RENDER_DPI` (большие форматы — мельче, чтобы уложиться в `MAX_SIDE`).
fn render(doc: &PdfDocument, index: u32) -> WinResult<SoftwareBitmap> {
    let page = doc.GetPage(index)?;
    // Размер страницы — в 1/96 дюйма.
    let size = page.Size()?;
    let scale = (RENDER_DPI as f32 / 96.0).min(MAX_SIDE as f32 / size.Width.max(size.Height).max(1.0));
    let options = PdfPageRenderOptions::new()?;
    options.SetDestinationWidth(((size.Width * scale).round() as u32).max(1))?;
    options.SetDestinationHeight(((size.Height * scale).round() as u32).max(1))?;
    // Без сжатия: картинка нужна только движку распознавания.
    options.SetBitmapEncoderId(BitmapEncoder::BmpEncoderId()?)?;
    let out = InMemoryRandomAccessStream::new()?;
    page.RenderWithOptionsToStreamAsync(&out, &options)?.join()?;
    out.Seek(0)?;
    let decoder = BitmapDecoder::CreateAsync(&out)?.join()?;
    bitmap(&decoder.GetFrameAsync(0)?.join()?, ExifOrientationMode::IgnoreExifOrientation)
}

impl Document {
    pub(crate) fn open(kind: SourceKind, bytes: &[u8]) -> Result<Self, OcrError> {
        let engines = engines()?;
        let stream = stream_of(bytes).map_err(|e| failed("файл не прочитан", e))?;
        let (source, pages) = match kind {
            SourceKind::Pdf => {
                let doc = PdfDocument::LoadFromStreamAsync(&stream).and_then(|op| op.join()).map_err(|e| failed("PDF не открылся", e))?;
                let pages = doc.PageCount().map_err(|e| failed("PDF не открылся", e))?;
                (Source::Pdf(doc), pages)
            }
            SourceKind::Image => {
                let decoder = BitmapDecoder::CreateAsync(&stream).and_then(|op| op.join()).map_err(|e| failed("картинка не открылась", e))?;
                let frames = decoder.FrameCount().map_err(|e| failed("картинка не открылась", e))?;
                (Source::Image(decoder), frames.max(1))
            }
        };
        Ok(Self { source, engines, pages })
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.pages
    }

    pub(crate) fn language(&self) -> &str {
        &self.engines.language
    }

    pub(crate) fn recognize(&self, page: u32) -> Result<String, OcrError> {
        if page == 0 || page > self.pages {
            return Err(OcrError::Failed(format!("страницы {page} нет: всего страниц {}", self.pages)));
        }
        let picture = match &self.source {
            Source::Pdf(doc) => render(doc, page - 1).map_err(|e| failed("страница не отрисована", e))?,
            Source::Image(decoder) => decoder.GetFrameAsync(page - 1).and_then(|op| op.join()).and_then(|frame| bitmap(&frame, ExifOrientationMode::RespectExifOrientation)).map_err(|e| failed("картинка не прочитана", e))?,
        };
        let read = |engine: &OcrEngine| engine.RecognizeAsync(&picture).and_then(|op| op.join()).and_then(|result| words_of(&result));
        let found = read(&self.engines.main).map_err(|e| failed("распознавание не удалось", e))?;
        // Второй проход — английским движком, если на странице есть латиница; не вышел — без него.
        let latin = found.iter().flatten().any(|w| w.text.chars().any(|c| c.is_ascii_alphabetic()));
        let english: Vec<Word> = match &self.engines.english {
            Some(engine) if latin => read(engine).map(|lines| lines.into_iter().flatten().collect()).unwrap_or_default(),
            _ => Vec::new(),
        };
        let lines: Vec<Line> = found
            .iter()
            .filter_map(|words| {
                let (first, last) = (words.first()?, words.last()?);
                let center = |w: &Word| (w.x + w.w / 2.0, w.y + w.h / 2.0);
                Some(Line {
                    text: refine_latin(words, &english).join(" "),
                    left: words.iter().map(|w| w.x).fold(f32::INFINITY, f32::min),
                    right: words.iter().map(|w| w.x + w.w).fold(f32::NEG_INFINITY, f32::max),
                    first: center(first),
                    last: center(last),
                    height: words.iter().map(|w| w.h).fold(0.0, f32::max),
                })
            })
            .collect();
        Ok(reading_order(&lines))
    }
}

pub(crate) fn language() -> Result<String, OcrError> {
    engines().map(|e| e.language)
}

pub(crate) fn pdf_page_count(bytes: &[u8]) -> Result<u32, OcrError> {
    let stream = stream_of(bytes).map_err(|e| failed("файл не прочитан", e))?;
    let doc = PdfDocument::LoadFromStreamAsync(&stream).and_then(|op| op.join()).map_err(|e| failed("PDF не открылся", e))?;
    doc.PageCount().map_err(|e| failed("PDF не открылся", e))
}
