//! Реализация на средствах Windows: `Windows.Data.Pdf` рисует страницу PDF в картинку,
//! `Windows.Graphics.Imaging` читает картинки (в том числе многостраничный TIFF) и приводит их к
//! виду, который принимает распознавание, `Windows.Media.Ocr` читает текст. Все вызовы ждут
//! результата на своём потоке (`join`), поэтому звать их нужно из блокирующего потока.

use crate::{reading_order, refine_latin, Line, OcrError, SourceKind, Word, MAX_SIDE, RENDER_DPI};
use windows::core::{Error, HSTRING};
use windows::Foundation::{PropertyType, PropertyValue};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapFrame, BitmapInterpolationMode, BitmapPixelFormat, BitmapPropertySet, BitmapTransform, BitmapTypedValue, ColorManagementMode, ExifOrientationMode, SoftwareBitmap};
use windows::Media::Ocr::{OcrEngine, OcrResult};
use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};

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

/// Кадр картинки в виде, который принимают распознавание и сжатие: BGRA, длинная сторона не
/// больше `max_side`; у фотографий учитывается поворот из EXIF.
fn bitmap(frame: &BitmapFrame, exif: ExifOrientationMode, max_side: u32) -> WinResult<SoftwareBitmap> {
    let (width, height) = (frame.PixelWidth()?.max(1), frame.PixelHeight()?.max(1));
    let k = (f64::from(max_side.max(1)) / f64::from(width.max(height))).min(1.0);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(((f64::from(width) * k).round() as u32).max(1))?;
    transform.SetScaledHeight(((f64::from(height) * k).round() as u32).max(1))?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    frame.GetSoftwareBitmapTransformedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied, &transform, exif, ColorManagementMode::DoNotColorManage)?.join()
}

/// Во сколько раз система увеличивает запрошенный размер отрисовки: он задаётся в единицах,
/// не зависящих от экрана, и на экране с масштабом 175 % картинка выходит в 1,75 раза больше.
/// Измеряется первой отрисовкой; дальше запрос делится на это число, чтобы не рисовать лишнего.
static RENDER_SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();

/// Страница PDF картинкой в `dpi` (большие форматы — мельче, чтобы уложиться в `max_side`):
/// ровно того размера в пикселях, который из этого следует.
fn render(doc: &PdfDocument, index: u32, dpi: u32, max_side: u32) -> WinResult<SoftwareBitmap> {
    let page = doc.GetPage(index)?;
    // Размер страницы — в 1/96 дюйма.
    let size = page.Size()?;
    let scale = (dpi.max(1) as f32 / 96.0).min(max_side.max(1) as f32 / size.Width.max(size.Height).max(1.0));
    let (width, height) = (((size.Width * scale).round() as u32).max(1), ((size.Height * scale).round() as u32).max(1));
    let system = RENDER_SCALE.get().copied().unwrap_or(1.0);
    let asked = ((width as f32 / system).round() as u32).max(1);
    let options = PdfPageRenderOptions::new()?;
    options.SetDestinationWidth(asked)?;
    options.SetDestinationHeight(((height as f32 / system).round() as u32).max(1))?;
    // Без сжатия: дальше картинку читает распознавание или сжимает свой кодировщик.
    options.SetBitmapEncoderId(BitmapEncoder::BmpEncoderId()?)?;
    let out = InMemoryRandomAccessStream::new()?;
    page.RenderWithOptionsToStreamAsync(&out, &options)?.join()?;
    out.Seek(0)?;
    let frame = BitmapDecoder::CreateAsync(&out)?.join()?.GetFrameAsync(0)?.join()?;
    if RENDER_SCALE.get().is_none() {
        let measured = frame.PixelWidth()? as f32 / asked as f32;
        let _ = RENDER_SCALE.set(if (0.5..=4.0).contains(&measured) { measured } else { 1.0 });
    }
    // Что бы ни нарисовала система, наружу идёт запрошенный размер.
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(width)?;
    transform.SetScaledHeight(height)?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    frame.GetSoftwareBitmapTransformedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied, &transform, ExifOrientationMode::IgnoreExifOrientation, ColorManagementMode::DoNotColorManage)?.join()
}

/// Файл из памяти: PDF или картинка, и сколько в нём страниц (кадров).
fn open_source(kind: SourceKind, bytes: &[u8]) -> Result<(Source, u32), OcrError> {
    let stream = stream_of(bytes).map_err(|e| failed("файл не прочитан", e))?;
    Ok(match kind {
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
    })
}

/// Страница источника картинкой: у PDF — отрисовка в `dpi`, у картинки — её кадр.
fn page_bitmap(source: &Source, index: u32, dpi: u32, max_side: u32) -> Result<SoftwareBitmap, OcrError> {
    match source {
        Source::Pdf(doc) => render(doc, index, dpi, max_side).map_err(|e| failed("страница не отрисована", e)),
        Source::Image(decoder) => decoder.GetFrameAsync(index).and_then(|op| op.join()).and_then(|frame| bitmap(&frame, ExifOrientationMode::RespectExifOrientation, max_side)).map_err(|e| failed("картинка не прочитана", e)),
    }
}

/// Картинка в JPEG с качеством `quality` (1–100).
fn encode_jpeg(picture: &SoftwareBitmap, quality: u8) -> WinResult<Vec<u8>> {
    let stream = InMemoryRandomAccessStream::new()?;
    let options = BitmapPropertySet::new()?;
    let value = BitmapTypedValue::Create(&PropertyValue::CreateSingle(f32::from(quality.clamp(1, 100)) / 100.0)?, PropertyType::Single)?;
    options.Insert(&HSTRING::from("ImageQuality"), &value)?;
    let encoder = BitmapEncoder::CreateWithEncodingOptionsAsync(BitmapEncoder::JpegEncoderId()?, &stream, &options)?.join()?;
    // JPEG прозрачности не знает.
    encoder.SetSoftwareBitmap(&SoftwareBitmap::ConvertWithAlpha(picture, BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore)?)?;
    encoder.FlushAsync()?.join()?;
    let size = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    reader.LoadAsync(size)?.join()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// Файл, открытый для показа страниц картинками (без распознавания).
pub(crate) struct Pictures {
    source: Source,
    pages: u32,
}

impl Pictures {
    pub(crate) fn open(kind: SourceKind, bytes: &[u8]) -> Result<Self, OcrError> {
        let (source, pages) = open_source(kind, bytes)?;
        Ok(Self { source, pages })
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.pages
    }

    pub(crate) fn jpeg(&self, page: u32, dpi: u32, max_side: u32, quality: u8) -> Result<crate::Picture, OcrError> {
        if page == 0 || page > self.pages {
            return Err(OcrError::Failed(format!("страницы {page} нет: всего страниц {}", self.pages)));
        }
        let picture = page_bitmap(&self.source, page - 1, dpi, max_side)?;
        let (width, height) = (picture.PixelWidth().unwrap_or(0).max(0) as u32, picture.PixelHeight().unwrap_or(0).max(0) as u32);
        let jpeg = encode_jpeg(&picture, quality).map_err(|e| failed("картинка не сжата", e))?;
        Ok(crate::Picture { jpeg, width, height })
    }
}

impl Document {
    pub(crate) fn open(kind: SourceKind, bytes: &[u8]) -> Result<Self, OcrError> {
        let engines = engines()?;
        let (source, pages) = open_source(kind, bytes)?;
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
        let picture = page_bitmap(&self.source, page - 1, RENDER_DPI, MAX_SIDE)?;
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
