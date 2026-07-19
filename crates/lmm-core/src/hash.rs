//! SHA-256 helpers. Content hashes are lmm's ground truth for "is this file
//! the one we put there": deployment, verification and safe deletion all
//! compare hashes before acting.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{IoContext, Result};

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).path_ctx(path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).path_ctx(path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Writer adapter that hashes everything written through it.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    pub bytes: u64,
}

impl<W: std::io::Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    pub fn finish(self) -> (W, String, u64) {
        (self.inner, hex(&self.hasher.finalize()), self.bytes)
    }
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hashing_writer_matches_file_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let mut w = HashingWriter::new(File::create(&p).unwrap());
        std::io::Write::write_all(&mut w, b"hello world").unwrap();
        let (_, hash, bytes) = w.finish();
        assert_eq!(bytes, 11);
        assert_eq!(hash, sha256_file(&p).unwrap());
    }
}
