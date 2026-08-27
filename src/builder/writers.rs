use sha2::{Digest as dg, Sha256};
use std::io::{self, Write};

use crate::digest::finish_sha256;
use oci_spec::image::Digest;
use tokio_util::sync::CancellationToken;

pub struct CancelLableWriter<W> {
    inner: W,
    cancellation: CancellationToken,
}

impl<W> CancelLableWriter<W> {
    pub fn new(inner: W, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            // `Write::write_all` retries `Interrupted` forever, so cancellation
            // must use a terminal error kind
            Err(io::Error::other("layer build cancelled"))
        } else {
            Ok(())
        }
    }
}

impl<W: Write> Write for CancelLableWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check_cancelled()?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_cancelled()?;
        self.inner.flush()
    }
}

pub struct HashingWriter<W> {
    inner_writer: W,
    hasher: Sha256,
    written_bytes_count: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner_writer: W) -> Self {
        Self {
            inner_writer,
            hasher: Sha256::new(),
            written_bytes_count: 0,
        }
    }

    pub fn finalize(self) -> (Digest, u64) {
        let digest = finish_sha256(self.hasher);
        (digest, self.written_bytes_count)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner_writer.write_all(buf)?;
        self.hasher.update(buf);
        self.written_bytes_count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner_writer.flush()
    }
}

pub struct SplitWriter<A, B> {
    first_writer: A,
    second_writer: B,
}

impl<A, B> SplitWriter<A, B> {
    pub fn new(first_writer: A, second_writer: B) -> Self {
        Self {
            first_writer,
            second_writer,
        }
    }

    pub fn into_inner(self) -> (A, B) {
        (self.first_writer, self.second_writer)
    }
}

impl<A: Write, B: Write> Write for SplitWriter<A, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.first_writer.write_all(buf)?;
        self.second_writer.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.first_writer.flush()?;
        self.second_writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CancelAfterPartialWrite {
        bytes: Vec<u8>,
        cancellation: CancellationToken,
    }

    impl Write for CancelAfterPartialWrite {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let written = buffer.len().min(1);
            self.bytes.extend_from_slice(&buffer[..written]);
            self.cancellation.cancel();
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancellable_writer_interrupts_writes() {
        let cancellation = CancellationToken::new();
        let mut writer = CancelLableWriter::new(Vec::new(), cancellation.clone());
        writer.write_all(b"before").unwrap();

        cancellation.cancel();

        let error = writer.write_all(b"after").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(writer.into_inner(), b"before");
    }

    #[test]
    fn cancellable_writer_stops_an_in_progress_write_all() {
        let cancellation = CancellationToken::new();
        let inner = CancelAfterPartialWrite {
            bytes: Vec::new(),
            cancellation: cancellation.clone(),
        };
        let mut writer = CancelLableWriter::new(inner, cancellation);

        let error = writer.write_all(b"payload").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(writer.into_inner().bytes, b"p");
    }
}
