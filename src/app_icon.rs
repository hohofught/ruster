use eframe::egui;

const DEFAULT_ICON_SIZE: usize = 64;
const GLYPH_WIDTH: usize = 5;
const GLYPH_HEIGHT: usize = 7;
const BLUE: [u8; 4] = [29, 78, 216, 255];

const RTR_R: [&str; GLYPH_HEIGHT] = [
    "11110", "10001", "10001", "11110", "10100", "10010", "10001",
];
const RTR_T: [&str; GLYPH_HEIGHT] = [
    "11111", "00100", "00100", "00100", "00100", "00100", "00100",
];

pub fn rtr_egui_icon_data() -> egui::IconData {
    egui::IconData {
        rgba: rtr_icon_rgba(DEFAULT_ICON_SIZE),
        width: DEFAULT_ICON_SIZE as u32,
        height: DEFAULT_ICON_SIZE as u32,
    }
}

pub fn rtr_icon_rgba(size: usize) -> Vec<u8> {
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

    for scale in (1..=8).rev() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtr_icon_keeps_transparent_background() {
        let rgba = rtr_icon_rgba(64);
        assert_eq!(rgba.len(), 64 * 64 * 4);
        assert_eq!(rgba[3], 0);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel == BLUE));
        assert!(
            rgba.chunks_exact(4).filter(|pixel| pixel[3] == 0).count()
                > rgba.chunks_exact(4).filter(|pixel| pixel[3] != 0).count()
        );
    }

    #[test]
    fn rtr_icon_small_size_still_draws_letters() {
        let rgba = rtr_icon_rgba(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel == BLUE));
    }
}
