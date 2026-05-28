use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const GLYPH_WIDTH: usize = 5;
const GLYPH_HEIGHT: usize = 7;
const BLUE: [u8; 4] = [29, 78, 216, 255];

const RTR_R: [&str; GLYPH_HEIGHT] = [
    "11110", "10001", "10001", "11110", "10100", "10010", "10001",
];
const RTR_T: [&str; GLYPH_HEIGHT] = [
    "11111", "00100", "00100", "00100", "00100", "00100", "00100",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    if let Err(error) = embed_windows_icon(&target) {
        println!("cargo:warning=Windows icon resource was not embedded: {error}");
    }
}

fn embed_windows_icon(target: &str) -> io::Result<()> {
    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is not available"))?,
    );
    let icon_path = out_dir.join("ruster.ico");
    let rc_path = out_dir.join("ruster.rc");
    let res_path = out_dir.join("ruster.res");

    fs::write(&icon_path, build_ico_file())?;
    fs::write(
        &rc_path,
        format!("1 ICON \"{}\"\n", escape_rc_path(&icon_path)),
    )?;

    let windres_target = windres_target(target).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported windres target: {target}"),
        )
    })?;
    let status = Command::new("windres.exe")
        .arg("--target")
        .arg(windres_target)
        .arg("--output-format=coff")
        .arg(&rc_path)
        .arg(&res_path)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("windres failed with {status}")));
    }

    println!("cargo:rustc-link-arg-bin=ruster={}", res_path.display());
    Ok(())
}

fn windres_target(target: &str) -> Option<&'static str> {
    if target.starts_with("x86_64-") {
        Some("pe-x86-64")
    } else if target.starts_with("i686-") {
        Some("pe-i386")
    } else {
        None
    }
}

fn escape_rc_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn build_ico_file() -> Vec<u8> {
    let sizes = [16usize, 24, 32, 48, 64, 256];
    let images: Vec<(usize, Vec<u8>)> = sizes
        .iter()
        .map(|size| (*size, build_icon_dib(*size)))
        .collect();

    let header_size = 6 + images.len() * 16;
    let mut bytes =
        Vec::with_capacity(header_size + images.iter().map(|(_, data)| data.len()).sum::<usize>());
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, images.len() as u16);

    let mut offset = header_size as u32;
    for (size, image) in &images {
        bytes.push(if *size >= 256 { 0 } else { *size as u8 });
        bytes.push(if *size >= 256 { 0 } else { *size as u8 });
        bytes.push(0);
        bytes.push(0);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 32);
        push_u32(&mut bytes, image.len() as u32);
        push_u32(&mut bytes, offset);
        offset += image.len() as u32;
    }

    for (_, image) in images {
        bytes.extend_from_slice(&image);
    }
    bytes
}

fn build_icon_dib(size: usize) -> Vec<u8> {
    let rgba = rtr_icon_rgba(size);
    let mask_stride = size.div_ceil(32) * 4;
    let image_size = size * size * 4;
    let mask_size = mask_stride * size;
    let mut bytes = Vec::with_capacity(40 + image_size + mask_size);

    push_u32(&mut bytes, 40);
    push_i32(&mut bytes, size as i32);
    push_i32(&mut bytes, (size * 2) as i32);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 32);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, image_size as u32);
    push_i32(&mut bytes, 0);
    push_i32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);

    for y in (0..size).rev() {
        for x in 0..size {
            let offset = (y * size + x) * 4;
            bytes.push(rgba[offset + 2]);
            bytes.push(rgba[offset + 1]);
            bytes.push(rgba[offset]);
            bytes.push(rgba[offset + 3]);
        }
    }
    bytes.resize(bytes.len() + mask_size, 0);
    bytes
}

fn rtr_icon_rgba(size: usize) -> Vec<u8> {
    let mut rgba = vec![0; size * size * 4];
    if size == 0 {
        return rgba;
    }

    let (scale, gap) = icon_layout(size);
    let text_width = (GLYPH_WIDTH * 3 + gap * 2) * scale;
    let text_height = GLYPH_HEIGHT * scale;
    let start_x = (size.saturating_sub(text_width)) / 2;
    let start_y = (size.saturating_sub(text_height)) / 2;

    for (index, glyph) in [RTR_R, RTR_T, RTR_R].iter().enumerate() {
        let glyph_x = start_x + index * (GLYPH_WIDTH + gap) * scale;
        draw_icon_glyph(&mut rgba, size, glyph_x, start_y, glyph, scale, BLUE);
    }

    rgba
}

fn icon_layout(size: usize) -> (usize, usize) {
    let padding = if size <= 32 { 1 } else { (size / 12).max(2) };
    let max_width = size.saturating_sub(padding * 2);
    let max_height = size.saturating_sub(padding * 2);

    for scale in (1..=32).rev() {
        for gap in [1, 0] {
            let text_width = (GLYPH_WIDTH * 3 + gap * 2) * scale;
            let text_height = GLYPH_HEIGHT * scale;
            if text_width <= max_width && text_height <= max_height {
                return (scale, gap);
            }
        }
    }

    (1, 0)
}

fn draw_icon_glyph(
    rgba: &mut [u8],
    canvas_width: usize,
    x: usize,
    y: usize,
    glyph: &[&str; GLYPH_HEIGHT],
    scale: usize,
    color: [u8; 4],
) {
    for (row, line) in glyph.iter().enumerate() {
        for (col, ch) in line.bytes().enumerate() {
            if ch != b'1' {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = x + col * scale + dx;
                    let py = y + row * scale + dy;
                    let offset = (py * canvas_width + px) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        }
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
