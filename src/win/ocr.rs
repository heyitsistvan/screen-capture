//! Word boxes from the OCR engine built into Windows (Windows.Media.Ocr).

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

use crate::ocr;
use crate::util::Rect;

/// Lines of word boxes found in a BGRA image (all passes), or `None` if the
/// engine isn't available.
pub fn recognize(bgra: &[u8], w: i32, h: i32) -> Option<Vec<Vec<Rect>>> {
    let data = bgra.to_vec();
    // WinRT needs a multithreaded apartment; the UI thread must stay
    // single-threaded for the common dialogs, so run on a short-lived thread.
    std::thread::spawn(move || unsafe {
        let init = RoInitialize(RO_INIT_MULTITHREADED).is_ok();
        let result = detect(&data, w, h).ok();
        if init {
            RoUninitialize();
        }
        result
    })
    .join()
    .ok()
    .flatten()
}

fn engine() -> windows::core::Result<OcrEngine> {
    OcrEngine::TryCreateFromUserProfileLanguages()
        .or_else(|_| OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en-US"))?))
}

fn detect(bgra: &[u8], w: i32, h: i32) -> windows::core::Result<Vec<Vec<Rect>>> {
    let engine = engine()?;
    let max = OcrEngine::MaxImageDimension()? as i32;
    let (scale, passes) = ocr::passes(bgra, w, h, max).ok_or_else(windows::core::Error::empty)?;
    let mut lines = Vec::new();
    for img in passes {
        let writer = DataWriter::new()?;
        writer.WriteBytes(&img)?;
        let bitmap =
            SoftwareBitmap::CreateCopyFromBuffer(&writer.DetachBuffer()?, BitmapPixelFormat::Bgra8, w * scale, h * scale)?;
        let result = engine.RecognizeAsync(&bitmap)?.get()?;
        for line in result.Lines()? {
            let mut words = Vec::new();
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                words.push(ocr::unscale(r.X, r.Y, r.Width, r.Height, scale));
            }
            lines.push(words);
        }
    }
    Ok(lines)
}
