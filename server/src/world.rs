//! Hex helpers for the per-version tables and tests.

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| s.get(2 * i..2 * i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()).unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_reads_pairs_and_does_not_panic_on_text_that_is_not_ascii() {
        assert_eq!(super::hex("00ff10"), vec![0, 255, 16]);
        assert_eq!(super::hex("zz01"), vec![0, 1]);
        // A multi-byte character in the middle of a pair is zeros, not a panic.
        assert_eq!(super::hex("aé01").len(), 2);
    }
}
