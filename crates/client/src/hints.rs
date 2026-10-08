//! Vimium-style hint labels: prefix-free, as short as the alphabet allows.

/// `n` distinct labels over `alphabet`, none a prefix of another.
pub fn labels(n: usize, alphabet: &str) -> Vec<String> {
    let chars: Vec<char> = alphabet.chars().collect();
    if n == 0 || chars.len() < 2 {
        return Vec::new();
    }
    let mut hints = vec![String::new()];
    let mut offset = 0;
    while hints.len() - offset < n || hints.len() == 1 {
        let h = hints[offset].clone();
        offset += 1;
        for c in &chars {
            hints.push(format!("{c}{h}"));
        }
    }
    // children were built by *prepending*; reversing makes extension an append, which is what
    // keeps the surviving set prefix-free (a removed parent never reappears as a prefix)
    hints[offset..offset + n]
        .iter()
        .map(|h| h.chars().rev().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_free_unique_and_short() {
        for n in [1, 2, 5, 26, 27, 60, 700, 1000] {
            let l = labels(n, "asdfghjklqwertyuiopzxcvbnm");
            assert_eq!(l.len(), n);
            let set: std::collections::HashSet<_> = l.iter().collect();
            assert_eq!(set.len(), n, "unique for {n}");
            for a in &l {
                for b in &l {
                    assert!(
                        a == b || !b.starts_with(a.as_str()),
                        "{a} is a prefix of {b} (n={n})"
                    );
                }
            }
            let max = l.iter().map(|s| s.len()).max().unwrap();
            assert!(
                max <= if n <= 26 {
                    1
                } else if n <= 26 * 26 {
                    2
                } else {
                    3
                },
                "n={n} max={max}"
            );
        }
    }

    #[test]
    fn degenerate_inputs() {
        assert!(labels(0, "abc").is_empty());
        assert!(labels(3, "a").is_empty());
    }
}
