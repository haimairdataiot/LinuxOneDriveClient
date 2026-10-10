//! QuickXorHash: the content hash OneDrive reports for every file
//! (`file.hashes.quickXorHash`), available on personal and business drives.
//!
//! Port of Microsoft's reference implementation: each input byte is XORed
//! into a 160-bit circular register at bit position `(index * 11) mod 160`;
//! the final digest is that register (little endian) with the total length
//! (64-bit little endian) XORed into its last 8 bytes.

use base64::Engine as _;

const WIDTH_BITS: usize = 160;
const SHIFT: usize = 11;
const CELLS: usize = 3; // 64 + 64 + 32 bits
const BITS_IN_LAST_CELL: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct QuickXorHash {
    data: [u64; CELLS],
    shift_so_far: usize,
    length: u64,
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, input: &[u8]) {
        let mut vector_index = self.shift_so_far / 64;
        let mut vector_offset = self.shift_so_far % 64;
        let iterations = input.len().min(WIDTH_BITS);

        for i in 0..iterations {
            let is_last_cell = vector_index == CELLS - 1;
            let bits_in_cell = if is_last_cell { BITS_IN_LAST_CELL } else { 64 };

            if vector_offset <= bits_in_cell - 8 {
                // The byte fits entirely inside the current cell.
                let mut j = i;
                while j < input.len() {
                    self.data[vector_index] ^= (input[j] as u64) << vector_offset;
                    j += WIDTH_BITS;
                }
            } else {
                // The byte straddles two cells (wrapping from the last to the first).
                let index2 = if is_last_cell { 0 } else { vector_index + 1 };
                let low = bits_in_cell - vector_offset;
                let mut xored: u8 = 0;
                let mut j = i;
                while j < input.len() {
                    xored ^= input[j];
                    j += WIDTH_BITS;
                }
                self.data[vector_index] ^= (xored as u64) << vector_offset;
                self.data[index2] ^= (xored as u64) >> low;
            }

            vector_offset += SHIFT;
            while vector_offset >= bits_in_cell {
                vector_index = if is_last_cell { 0 } else { vector_index + 1 };
                vector_offset -= bits_in_cell;
            }
        }

        self.shift_so_far = (self.shift_so_far + SHIFT * (input.len() % WIDTH_BITS)) % WIDTH_BITS;
        self.length += input.len() as u64;
    }

    /// The 20-byte digest.
    pub fn digest(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..8].copy_from_slice(&self.data[0].to_le_bytes());
        out[8..16].copy_from_slice(&self.data[1].to_le_bytes());
        out[16..20].copy_from_slice(&self.data[2].to_le_bytes()[..4]);
        let len = self.length.to_le_bytes();
        for (i, b) in len.iter().enumerate() {
            out[20 - 8 + i] ^= b;
        }
        out
    }

    /// Base64 digest, the form used by the Graph API.
    pub fn finalize_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.digest())
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

/// Hash a byte slice.
pub fn hash_bytes(data: &[u8]) -> String {
    let mut h = QuickXorHash::new();
    h.update(data);
    h.finalize_base64()
}

/// Hash a file from disk (blocking; call from `spawn_blocking`).
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = QuickXorHash::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize_base64())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straightforward bit-by-bit definition used to cross-check the optimized port.
    fn reference(data: &[u8]) -> [u8; 20] {
        let mut reg = [0u8; 20]; // 160-bit little-endian register
        for (k, &byte) in data.iter().enumerate() {
            let pos = (k * SHIFT) % WIDTH_BITS;
            for bit in 0..8 {
                if byte & (1 << bit) != 0 {
                    let p = (pos + bit) % WIDTH_BITS;
                    reg[p / 8] ^= 1 << (p % 8);
                }
            }
        }
        let len = (data.len() as u64).to_le_bytes();
        for i in 0..8 {
            reg[12 + i] ^= len[i];
        }
        reg
    }

    #[test]
    fn empty_and_single_byte() {
        assert_eq!(hash_bytes(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(hash_bytes(&[0x4a]), "SgAAAAAAAAAAAAAAAQAAAAAAAAA=");
    }

    #[test]
    fn matches_reference_definition() {
        let mut seed: u32 = 0x1234_5678;
        let mut data = vec![0u8; 5000];
        for b in data.iter_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *b = (seed >> 16) as u8;
        }
        for len in [0usize, 1, 7, 8, 19, 20, 159, 160, 161, 319, 320, 321, 999, 5000] {
            let mut h = QuickXorHash::new();
            h.update(&data[..len]);
            assert_eq!(h.digest(), reference(&data[..len]), "len {len}");
        }
    }

    #[test]
    fn chunking_is_irrelevant() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = hash_bytes(&data);
        for chunk in [1usize, 3, 64, 159, 160, 161, 1000, 4096] {
            let mut h = QuickXorHash::new();
            for c in data.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.finalize_base64(), whole, "chunk {chunk}");
        }
    }
}
