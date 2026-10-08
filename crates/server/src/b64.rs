//! Tiny standard-alphabet base64 decoder (CDP screenshots); avoids a dependency for 20 lines.

use anyhow::{bail, Result};

pub fn decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => bail!("invalid base64 byte {b:#x}"),
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes() {
        assert_eq!(super::decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(super::decode("aGVsbG8h").unwrap(), b"hello!");
        assert!(super::decode("a$b").is_err());
    }
}
