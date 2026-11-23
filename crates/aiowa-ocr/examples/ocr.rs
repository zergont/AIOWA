//! Проба распознавания на файле: `cargo run -p aiowa-ocr --example ocr -- <файл> [--stats]`.
//! Печатает текст страниц; с `--stats` — только число страниц, символов и время (для чужих
//! документов, содержимое которых в консоль попадать не должно).

use aiowa_ocr::{source_kind, Document};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stats = args.iter().any(|a| a == "--stats");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("нужен путь к файлу PDF или картинки");
        std::process::exit(2);
    };
    let Some(kind) = source_kind(Some(path), None) else {
        eprintln!("такие файлы не распознаются: нужен PDF, JPG, PNG, TIFF или BMP");
        std::process::exit(2);
    };
    let bytes = std::fs::read(path).expect("файл не прочитан");
    let started = Instant::now();
    let doc = Document::open(kind, &bytes).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    eprintln!("страниц: {}, язык распознавания: {}, открытие: {} мс", doc.page_count(), doc.language(), started.elapsed().as_millis());
    for page in 1..=doc.page_count() {
        let t = Instant::now();
        match doc.recognize(page) {
            Ok(text) if stats => eprintln!("стр. {page}: {} символов, {} строк, {} мс", text.chars().count(), text.lines().count(), t.elapsed().as_millis()),
            Ok(text) => {
                eprintln!("стр. {page}: {} мс", t.elapsed().as_millis());
                println!("--- стр. {page} ---\n{text}");
            }
            Err(e) => eprintln!("стр. {page}: {e}"),
        }
    }
}
