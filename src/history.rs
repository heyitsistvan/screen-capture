//! The last few captures, kept in memory only (gone when the app exits), for
//! the tray menu: each can be pinned again to compare it with the screen or
//! with a newer capture. Stored as PNG so ten full-screen captures don't keep
//! a hundred-odd megabytes alive.

use std::collections::VecDeque;

use crate::util;

pub const MAX: usize = 10;
/// Largest thumbnail kept; menus scale it down to fit.
pub const THUMB_W: i32 = 96;
pub const THUMB_H: i32 = 64;

pub struct Entry {
    /// Assigned when added to the history.
    pub id: u64,
    /// When it was captured, `HH:MM:SS`.
    pub clock: String,
    pub w: i32,
    pub h: i32,
    /// Where it was on screen, so a pin opens right over the same spot.
    pub at: (i32, i32),
    /// Small BGRA preview `(w, h, pixels)` (Windows menus take images that way).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub thumb: (i32, i32, Vec<u8>),
    /// The same preview as PNG (Linux tray menus take images that way).
    #[cfg_attr(windows, allow(dead_code))]
    pub thumb_png: Vec<u8>,
    png: Vec<u8>,
}

impl Entry {
    /// Compresses a capture for keeping. Takes a moment for big captures, so
    /// call it off the UI thread.
    pub fn new(w: i32, h: i32, bgra: &[u8], at: (i32, i32)) -> Option<Entry> {
        let png = util::encode_png(w, h, bgra).ok()?;
        let (tw, th) = util::fit(w, h, THUMB_W, THUMB_H);
        let small = util::resize(bgra, w, h, tw, th);
        let thumb_png = util::encode_png(tw, th, &small).ok()?;
        Some(Entry { id: 0, clock: util::clock(), w, h, at, thumb: (tw, th, small), thumb_png, png })
    }

    /// The full image, BGRA.
    pub fn image(&self) -> Option<(i32, i32, Vec<u8>)> {
        util::decode_png(&self.png).ok()
    }

    /// Menu text, e.g. `14:32:05 · 800 × 450`.
    pub fn label(&self) -> String {
        format!("{} · {} × {}", self.clock, self.w, self.h)
    }
}

#[derive(Default)]
pub struct History {
    entries: VecDeque<Entry>,
    next_id: u64,
}

impl History {
    /// Adds a capture as the newest; the oldest drops off past [`MAX`].
    pub fn push(&mut self, mut e: Entry) -> u64 {
        self.next_id += 1;
        e.id = self.next_id;
        self.entries.push_front(e);
        self.entries.truncate(MAX);
        self.next_id
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    #[cfg_attr(not(windows), allow(dead_code))] // the Linux tray lists entries instead
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Keeps the newest ten, round-trips images exactly (debug self-test).
#[cfg(debug_assertions)]
pub fn selftest() -> Result<String, String> {
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };
    let (w, h) = (300, 120);
    let img = |seed: u8| -> Vec<u8> { (0..w * h).flat_map(|i| [seed, (i % 256) as u8, (i / w % 256) as u8, 255]).collect() };
    let mut hist = History::default();
    for i in 0..12u8 {
        hist.push(Entry::new(w, h, &img(i), (i as i32, 0)).ok_or("couldn't compress a capture")?);
    }
    check(hist.iter().count() == MAX, "history isn't capped at 10")?;
    let newest = hist.iter().next().ok_or("empty history")?;
    check(newest.at == (11, 0) && hist.iter().last().is_some_and(|e| e.at == (2, 0)), "not newest first, or the oldest weren't dropped")?;
    check(hist.get(newest.id).is_some(), "lookup by id failed")?;
    check(newest.image() == Some((w, h, img(11))), "the image didn't come back exactly")?;
    check(newest.thumb.0 <= THUMB_W && newest.thumb.1 <= THUMB_H && newest.thumb.0 == THUMB_W, "thumbnail size")?;
    check(newest.label().ends_with("300 × 120"), "menu label")?;
    hist.clear();
    check(hist.is_empty(), "clear didn't empty it")?;
    Ok("keeps the newest 10, exact round-trip, thumbnails, clear".into())
}
