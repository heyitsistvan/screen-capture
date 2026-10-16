//! Generates the application icon and embeds it, together with the manifest
//! (per-monitor DPI awareness, common controls v6), into the executable.

#[path = "src/icon.rs"]
#[allow(dead_code)]
mod icon;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=res/app.manifest");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    fs::write(&ico, build_ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("res")
        .join("app.manifest");
    let rc = out.join("app.rc");
    fs::write(
        &rc,
        format!("1 ICON \"{}\"\n1 24 \"{}\"\n", rc_path(&ico), rc_path(&manifest)),
    )
    .unwrap();
    let _ = embed_resource::compile(&rc, embed_resource::NONE);
}

fn rc_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

/// Builds a multi-resolution .ico: classic 32-bit DIB entries for the small
/// sizes and a PNG-compressed entry for 256px.
fn build_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let rgba = icon::render_rgba(s, false);
            if s >= 256 {
                encode_png(s, &rgba)
            } else {
                encode_dib(s, &rgba)
            }
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (&s, img) in sizes.iter().zip(&images) {
        let d = if s >= 256 { 0 } else { s as u8 };
        out.extend_from_slice(&[d, d, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in &images {
        out.extend_from_slice(img);
    }
    out
}

fn encode_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // BITMAPINFOHEADER; height is doubled to account for the AND mask.
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    let mask_row = ((size + 31) / 32 * 4) as usize;
    out.extend(std::iter::repeat(0u8).take(mask_row * size as usize));
    out
}

fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(rgba).unwrap();
    }
    out
}
