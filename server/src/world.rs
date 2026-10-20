//! Bytes written as hex in the source and the tables' own files (a captured message, the
//! connection request): a typo is a panic that names the text, never a zero byte.

pub fn hex(s: &str) -> Vec<u8> {
    crate::util::from_hex(s).unwrap_or_else(|e| panic!("hex {s:?}: {e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_reads_pairs_and_a_typo_is_not_a_zero_byte() {
        assert_eq!(super::hex("00ff10"), vec![0, 255, 16]);
        for bad in ["zz01", "aé01", "abc"] {
            let said = std::panic::catch_unwind(|| super::hex(bad)).unwrap_err();
            let said = said.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(said.contains(bad) && said.contains("hex"), "{bad}: {said}");
        }
    }
}
