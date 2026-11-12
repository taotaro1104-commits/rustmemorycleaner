//! トレイアイコン（使用率を数字で描く）。小さなビットマップフォントで 32x32 の RGBA を作る。
pub const SIZE: usize = 32;
const CELL: usize = 5; // 3x5 のフォントを 5 倍 → 1 桁 15x25、2 桁で 32 幅に収まる

const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// 使用率に応じた背景色（RGB）: 緑 → 橙 → 赤。
pub fn background(percent: u8) -> [u8; 3] {
    match percent {
        0..=69 => [0x2e, 0x7d, 0x32],
        70..=84 => [0xe0, 0x8a, 0x00],
        _ => [0xc6, 0x28, 0x28],
    }
}

/// 0〜100 を 2 桁で表示（100 は 99 に丸める）。RGBA（SIZE x SIZE）を返す。
pub fn render(percent: u8) -> Vec<u8> {
    let p = percent.min(99);
    let bg = background(percent);
    let mut px = vec![0u8; SIZE * SIZE * 4];
    for i in 0..SIZE * SIZE {
        px[i * 4..i * 4 + 4].copy_from_slice(&[bg[0], bg[1], bg[2], 255]);
    }
    let digits = [(p / 10) as usize, (p % 10) as usize];
    let (w, h) = (3 * CELL, 5 * CELL);
    let x0 = (SIZE - (2 * w + 2)) / 2;
    let y0 = (SIZE - h) / 2;
    for (di, d) in digits.iter().enumerate() {
        for (row, bits) in DIGITS[*d].iter().enumerate() {
            for col in 0..3 {
                if bits >> (2 - col) & 1 == 1 {
                    for dy in 0..CELL {
                        for dx in 0..CELL {
                            let x = x0 + di * (w + 2) + col * CELL + dx;
                            let y = y0 + row * CELL + dy;
                            px[(y * SIZE + x) * 4..(y * SIZE + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
            }
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_expected_size_and_is_opaque() {
        let px = render(42);
        assert_eq!(px.len(), SIZE * SIZE * 4);
        assert!(px.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn different_percents_render_differently_and_same_is_stable() {
        assert_ne!(render(12), render(13));
        assert_eq!(render(55), render(55));
    }

    #[test]
    fn digits_are_drawn_in_white_on_colored_background() {
        let px = render(88);
        assert!(px.chunks(4).any(|p| p[..3] == [255, 255, 255]));
        assert_eq!(&px[..3], &background(88));
    }

    #[test]
    fn color_thresholds() {
        assert_eq!(background(69), background(0));
        assert_ne!(background(70), background(69));
        assert_ne!(background(85), background(84));
    }

    #[test]
    fn hundred_does_not_overflow() {
        assert_eq!(render(100).len(), SIZE * SIZE * 4);
    }
}
