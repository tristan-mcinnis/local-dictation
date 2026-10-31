//! SHA-256, streaming, no dependencies.
//!
//! The model download has to prove that the bytes on disk are the bytes the
//! manifest names before the app loads them. That needs a digest, and the two
//! alternatives were both worse than eighty lines of FIPS 180-4: a crate adds
//! a dependency to a repo that is deliberately dependency-light, and shelling
//! out to `/usr/bin/shasum` means parsing another program's output on the one
//! path where being wrong matters.
//!
//! Streaming (`update` in chunks) so a 462 MB file is hashed without being
//! read into memory, and so the Setup window can show progress while it runs.

/// FIPS 180-4 round constants: the first 32 bits of the fractional parts of
/// the cube roots of the first 64 primes.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256 state.
pub struct Sha256 {
    h: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length_bits: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            // First 32 bits of the fractional parts of the square roots of the
            // first eight primes.
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffered: 0,
            length_bits: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add((data.len() as u64) * 8);
        if self.buffered > 0 {
            let want = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + want].copy_from_slice(&data[..want]);
            self.buffered += want;
            data = &data[want..];
            if self.buffered < 64 {
                // `data` is exhausted inside the partial block. Returning here
                // matters: the tail assignment below sets `buffered` from the
                // remainder of an empty slice, which would throw the partial
                // block away and leave `finish` spinning for a count it can
                // never reach.
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for block in blocks {
            self.compress(block);
        }
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    /// Finish and return the 32-byte digest.
    pub fn finish(mut self) -> [u8; 32] {
        let length_bits = self.length_bits;
        self.update_no_length(&[0x80]);
        while self.buffered != 56 {
            self.update_no_length(&[0x00]);
        }
        self.update_no_length(&length_bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// Lowercase hex digest.
    pub fn finish_hex(self) -> String {
        self.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Padding bytes must not count towards the message length.
    fn update_no_length(&mut self, data: &[u8]) {
        let saved = self.length_bits;
        self.update(data);
        self.length_bits = saved;
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in self
            .h
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h])
        {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Hash a whole file, reporting bytes read so a long verify can show progress
/// and be cancelled. `on_chunk` returning `false` aborts with `None`.
pub fn hash_file(
    path: &std::path::Path,
    mut on_chunk: impl FnMut(u64) -> bool,
) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut read_total: u64 = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        read_total += n as u64;
        if !on_chunk(read_total) {
            return Ok(None);
        }
    }
    Ok(Some(hasher.finish_hex()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        h.finish_hex()
    }

    #[test]
    fn matches_the_published_test_vectors() {
        // FIPS 180-4 / NIST CAVP known answers.
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Exactly one block, and one byte over: the padding edge cases.
        assert_eq!(
            hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            hex(&b"a".repeat(1_000_000)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Regression: feeding one byte at a time used to lose the partial block,
    /// so `buffered` never reached the padding boundary and `finish` looped
    /// forever. Byte-at-a-time is exactly what `finish`'s own padding does.
    #[test]
    fn streaming_in_any_chunk_size_gives_the_same_digest() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let once = hex(&data);
        for chunk in [1usize, 7, 63, 64, 65, 128, 1000] {
            let mut h = Sha256::new();
            for piece in data.chunks(chunk) {
                h.update(piece);
            }
            assert_eq!(h.finish_hex(), once, "chunk size {chunk}");
        }
    }

    #[test]
    fn hashing_a_file_reports_progress_and_can_be_cancelled() {
        let dir = std::env::temp_dir().join(format!("ld-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob.bin");
        std::fs::write(&path, b"abc").unwrap();

        let mut seen = Vec::new();
        let digest = hash_file(&path, |read| {
            seen.push(read);
            true
        })
        .unwrap();
        assert_eq!(digest.as_deref(), Some(&*hex(b"abc")));
        assert_eq!(seen, vec![3]);

        // A cancelled verify reports None rather than a wrong digest.
        assert!(hash_file(&path, |_| false).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
