//! AES-128-CBC with the `cbcs` pattern (ISO/IEC 23001-7, 10.4). A library CBC mode would chain
//! through the skipped blocks, so the chaining is done here over a raw block cipher.

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit};

/// How many 16-byte blocks are encrypted, then skipped, repeating across a protected range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub(crate) crypt: u8,
    pub(crate) skip: u8,
}

impl Pattern {
    /// Video in `cbcs`: one block in ten.
    pub(crate) const VIDEO: Self = Self { crypt: 1, skip: 9 };
    /// Audio in `cbcs`: no pattern; every whole block.
    pub(crate) const FULL: Self = Self { crypt: 0, skip: 0 };
}

pub(crate) struct Cipher(Aes128);

impl Cipher {
    pub(crate) fn new(key: &[u8; 16]) -> Self {
        Self(Aes128::new(&(*key).into()))
    }

    /// Encrypts one protected range in place, starting the chain from `iv`. Skipped blocks and a
    /// trailing partial block stay clear.
    pub(crate) fn encrypt_range(&self, iv: &[u8; 16], data: &mut [u8], pattern: Pattern) {
        let crypt = usize::from(pattern.crypt);
        let period = crypt + usize::from(pattern.skip);
        let mut chain = *iv;
        for (index, block) in data.as_chunks_mut::<16>().0.iter_mut().enumerate() {
            if period != 0 && index % period >= crypt {
                continue;
            }
            for (byte, previous) in block.iter_mut().zip(chain) {
                *byte ^= previous;
            }
            self.0.encrypt_block(block.into());
            chain = *block;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    // NIST SP 800-38A, F.2.1 (CBC-AES128.Encrypt).
    const KEY: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const IV: &str = "000102030405060708090a0b0c0d0e0f";
    const PLAIN: [&str; 4] = [
        "6bc1bee22e409f96e93d7e117393172a",
        "ae2d8a571e03ac9c9eb76fac45af8e51",
        "30c81c46a35ce411e5fbc1191a0a52ef",
        "f69f2445df4f9b17ad2b417be66c3710",
    ];
    const CIPHER: [&str; 4] = [
        "7649abac8119b246cee98e9b12e9197d",
        "5086cb9b507219ee95db113a917678b2",
        "73bed6b8e3c1743b7116e69e22229516",
        "3ff1caa1681fac09120eca307586e1a7",
    ];

    fn cipher() -> (Cipher, [u8; 16]) {
        (
            Cipher::new(&hex(KEY).try_into().unwrap()),
            hex(IV).try_into().unwrap(),
        )
    }

    #[test]
    fn with_no_pattern_it_is_plain_cbc_and_leaves_a_partial_block_clear() {
        let (cipher, iv) = cipher();
        let mut data = PLAIN
            .iter()
            .flat_map(|block| hex(block))
            .collect::<Vec<_>>();
        data.extend_from_slice(b"tail");

        cipher.encrypt_range(&iv, &mut data, Pattern::FULL);

        let expected = CIPHER
            .iter()
            .flat_map(|block| hex(block))
            .collect::<Vec<_>>();
        assert_eq!(&data[..64], expected.as_slice());
        assert_eq!(&data[64..], b"tail");
    }

    #[test]
    fn one_in_ten_chains_only_the_encrypted_blocks() {
        let (cipher, iv) = cipher();
        // Blocks 0 and 10 are encrypted; their chain skips the nine clear blocks between them,
        // so block 10 is encrypted exactly as the second block of plain CBC would be.
        let mut data = vec![0x55; 16 * 20];
        data[..16].copy_from_slice(&hex(PLAIN[0]));
        data[160..176].copy_from_slice(&hex(PLAIN[1]));
        let original = data.clone();

        cipher.encrypt_range(&iv, &mut data, Pattern::VIDEO);

        assert_eq!(&data[..16], hex(CIPHER[0]).as_slice());
        assert_eq!(&data[160..176], hex(CIPHER[1]).as_slice());
        for block in (1..20).filter(|block| *block != 10) {
            let range = block * 16..block * 16 + 16;
            assert_eq!(
                data[range.clone()],
                original[range],
                "block {block} stays clear"
            );
        }
    }
}
