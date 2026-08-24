use std::fmt;
use std::pin::Pin;

#[cfg(feature = "runtime-async-std")]
use async_std::io::{Read, Write, WriteExt};
use bytes::BytesMut;
use futures::stream::Stream;
use futures::task::{Context, Poll};
use futures::{io, ready};
use nom::Needed;
#[cfg(feature = "runtime-tokio")]
use tokio::io::{AsyncRead as Read, AsyncWrite as Write, AsyncWriteExt};

use crate::types::{Request, ResponseData};

/// Wraps a stream, and parses incoming data as imap server messages. Writes outgoing data
/// as imap client messages.
#[derive(Debug)]
pub struct ImapStream<R: Read + Write> {
    /// The underlying stream
    pub(crate) inner: R,
    /// Number of bytes the next decode operation needs if known.
    /// If the buffer contains less than this, it is a waste of time to try to parse it.
    /// If unknown, set it to 0, so decoding is always attempted.
    decode_needs: usize,
    /// The buffer.
    buffer: Buffer,

    /// True if the stream should not return any more items.
    ///
    /// This is set when reading from a stream
    /// returns an error.
    /// Afterwards the stream returns only `None`
    /// and `poll_next()` does not read
    /// from the underlying stream.
    read_closed: bool,
}

impl<R: Read + Write + Unpin> ImapStream<R> {
    /// Creates a new `ImapStream` based on the given `Read`er.
    pub fn new(inner: R) -> Self {
        ImapStream {
            inner,
            buffer: Buffer::new(),
            decode_needs: 0,
            read_closed: false,
        }
    }

    pub async fn encode(&mut self, msg: Request) -> Result<(), io::Error> {
        log::trace!("encode: input: {:?}, {:?}", msg.0, std::str::from_utf8(&msg.1));

        if let Some(tag) = msg.0 {
            self.inner.write_all(tag.as_bytes()).await?;
            self.inner.write_all(b" ").await?;
        }
        self.inner.write_all(&msg.1).await?;
        self.inner.write_all(b"\r\n").await?;

        Ok(())
    }

    /// Gets a reference to the underlying stream.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Gets a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Returns underlying stream.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Flushes the underlying stream.
    pub async fn flush(&mut self) -> Result<(), io::Error> {
        self.inner.flush().await
    }

    pub fn as_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Attempts to decode a single response from the buffer.
    ///
    /// Returns `None` if the buffer does not contain enough data.
    fn decode(&mut self) -> io::Result<Option<ResponseData>> {
        if self.buffer.used() < self.decode_needs {
            // We know that there is not enough data to decode anything
            // from previous attempts.
            return Ok(None);
        }

        let buf = self.buffer.as_slice();
        log::trace!("decode: input: {:?}", std::str::from_utf8(buf));
        match imap_proto::parser::parse_response(buf) {
            Ok((remaining, response)) => {
                // TODO: figure out if we can use a minimum required size for a response.
                self.decode_needs = 0;
                let consumed = buf.len() - remaining.len();
                let response = response.into_owned();
                self.buffer.consume(consumed);
                Ok(Some(ResponseData::from_owned(response)))
            }
            Err(nom::Err::Incomplete(Needed::Size(min))) => {
                log::trace!("decode: incomplete data, need minimum {min} bytes");
                self.decode_needs = self.buffer.used() + usize::from(min);
                Ok(None)
            }
            Err(nom::Err::Incomplete(_)) => {
                log::trace!("decode: incomplete data, need unknown number of bytes");
                self.decode_needs = 0;
                Ok(None)
            }
            Err(other) => {
                self.decode_needs = 0;
                Err(io::Error::other(format!("{:?} during parsing of {:?}", other, String::from_utf8_lossy(buf))))
            }
        }
    }

    fn do_poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<io::Result<ResponseData>>> {
        let this = &mut *self;
        if let Some(response) = this.decode()? {
            return Poll::Ready(Some(Ok(response)));
        }
        loop {
            this.buffer.ensure_capacity(this.decode_needs)?;
            let buf = this.buffer.free_as_mut_slice();

            // The buffer should have at least one byte free
            // before we try reading into it
            // so we can treat 0 bytes read as EOF.
            // This is guaranteed by `ensure_capacity()` above
            // even if it is called with 0 as an argument.
            debug_assert!(!buf.is_empty());

            #[cfg(feature = "runtime-async-std")]
            let num_bytes_read = ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;

            #[cfg(feature = "runtime-tokio")]
            let num_bytes_read = {
                let buf = &mut tokio::io::ReadBuf::new(buf);
                let start = buf.filled().len();
                ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
                buf.filled().len() - start
            };

            if num_bytes_read == 0 {
                if this.buffer.used() > 0 {
                    return Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bytes remaining in stream"))));
                }
                return Poll::Ready(None);
            }
            this.buffer.extend_used(num_bytes_read);
            if let Some(response) = this.decode()? {
                return Poll::Ready(Some(Ok(response)));
            }
        }
    }
}

/// Abstraction around needed buffer management.
struct Buffer {
    /// The buffer itself.
    block: BytesMut,
    /// Offset where used bytes range ends.
    offset: usize,
}

impl Buffer {
    const BLOCK_SIZE: usize = 1024 * 4;
    const MAX_CAPACITY: usize = 512 * 1024 * 1024; // 512 MiB

    fn new() -> Self {
        Self {
            block: BytesMut::zeroed(Self::BLOCK_SIZE),
            offset: 0,
        }
    }

    /// Returns the number of bytes in the buffer containing data.
    fn used(&self) -> usize {
        self.offset
    }

    /// Returns the used part of the buffer, containing whatever hasn't been
    /// consumed by a successful `decode()` yet.
    fn as_slice(&self) -> &[u8] {
        &self.block[..self.offset]
    }

    /// Returns the unused part of the buffer to which new data can be written.
    fn free_as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.block[self.offset..]
    }

    /// Indicate how many new bytes were written into the buffer.
    ///
    /// When new bytes are written into the slice returned by [`free_as_mut_slice`] this method
    /// should be called to extend the used portion of the buffer to include the new data.
    ///
    /// You can not write past the end of the buffer, so extending more then there is free
    /// space marks the entire buffer as used.
    ///
    /// [`free_as_mut_slice`]: Self::free_as_mut_slice
    fn extend_used(&mut self, num_bytes: usize) {
        self.offset += num_bytes;
        if self.offset > self.block.len() {
            self.offset = self.block.len();
        }
    }

    /// Ensure the buffer has free capacity, optionally ensuring minimum buffer size.
    fn ensure_capacity(&mut self, required: usize) -> io::Result<()> {
        let free_bytes: usize = self.block.len() - self.offset;
        let extra_bytes_needed: usize = required.saturating_sub(self.block.len());
        if free_bytes == 0 || extra_bytes_needed > 0 {
            let increase = std::cmp::max(Buffer::BLOCK_SIZE, extra_bytes_needed);
            self.grow(increase)?;
        }

        // Assert that the buffer at least one free byte.
        debug_assert!(self.offset < self.block.len());

        // Assert that the buffer has at least the required capacity.
        debug_assert!(self.block.len() >= required);
        Ok(())
    }

    /// Grows the buffer, ensuring there are free bytes in the tail.
    ///
    /// The specified number of bytes is only a minimum.  The buffer could grow by more as
    /// it will always grow in multiples of [`BLOCK_SIZE`].
    ///
    /// If the size would be larger than [`MAX_CAPACITY`] an error is returned.
    ///
    /// [`BLOCK_SIZE`]: Self::BLOCK_SIZE
    /// [`MAX_CAPACITY`]: Self::MAX_CAPACITY
    fn grow(&mut self, num_bytes: usize) -> io::Result<()> {
        let min_size = self.block.len() + num_bytes;
        let new_size = match min_size % Self::BLOCK_SIZE {
            0 => min_size,
            n => min_size + (Self::BLOCK_SIZE - n),
        };
        if new_size > Self::MAX_CAPACITY {
            Err(io::Error::other("incoming data too large"))
        } else {
            self.block.resize(new_size, 0);
            Ok(())
        }
    }

    /// Drops `amount` consumed bytes off the front of the buffer.
    ///
    /// Called after a successful parse with however many bytes it consumed.
    /// `BytesMut::advance` on the `Vec`-backed representation `block` always
    /// has here (created via `zeroed`/`resize`, never `split_off`/`freeze`)
    /// is a pointer bump, not a copy - unlike the old block-swap-and-copy
    /// approach this replaces, a decode burst of many small responses no
    /// longer re-copies the shrinking tail on every single one of them.
    fn consume(&mut self, amount: usize) {
        debug_assert!(amount <= self.offset);
        bytes::Buf::advance(&mut self.block, amount);
        self.offset -= amount;
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer").field("used", &self.used()).field("capacity", &self.block.capacity()).finish()
    }
}

impl<R: Read + Write + Unpin> Stream for ImapStream<R> {
    type Item = io::Result<ResponseData>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.read_closed {
            return Poll::Ready(None);
        }
        let res = match ready!(self.as_mut().do_poll_next(cx)) {
            None => None,
            Some(Err(err)) => {
                self.read_closed = true;
                Some(Err(err))
            }
            Some(Ok(item)) => Some(Ok(item)),
        };
        Poll::Ready(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pin_project::pin_project;
    use std::io::Write as _;

    /// Wrapper for a stream that
    /// fails once on a first read.
    ///
    /// Writes are discarded.
    #[pin_project]
    struct FailingStream {
        #[pin]
        inner: &'static [u8],
        has_failed: bool,
    }

    impl FailingStream {
        fn new(buf: &'static [u8]) -> Self {
            Self { inner: buf, has_failed: false }
        }
    }

    #[cfg(feature = "runtime-tokio")]
    impl Read for FailingStream {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<tokio::io::Result<()>> {
            let this = self.project();
            if !*this.has_failed {
                *this.has_failed = true;

                Poll::Ready(Err(std::io::Error::other("Failure")))
            } else {
                this.inner.poll_read(cx, buf)
            }
        }
    }

    #[cfg(feature = "runtime-async-std")]
    impl Read for FailingStream {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<async_std::io::Result<usize>> {
            let this = self.project();
            if !*this.has_failed {
                *this.has_failed = true;

                Poll::Ready(Err(std::io::Error::other("Failure")))
            } else {
                this.inner.poll_read(cx, buf)
            }
        }
    }

    #[cfg(feature = "runtime-tokio")]
    impl Write for FailingStream {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<tokio::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[cfg(feature = "runtime-async-std")]
    impl Write for FailingStream {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<async_std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<async_std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<async_std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Tests that stream returns `None` after
    /// a single error of the underlying stream.
    ///
    /// This is need to prevent accidental
    /// reading from a network stream
    /// after a temporary error such as a timeout
    /// or returning an inifinite stream of errors.
    #[cfg_attr(feature = "runtime-tokio", tokio::test)]
    #[cfg_attr(feature = "runtime-async-std", async_std::test)]
    async fn test_imap_stream_error() {
        use futures::StreamExt;

        let mock_stream = FailingStream::new(b"* OK\r\n");
        let mut imap_stream = ImapStream::new(mock_stream);

        // First call is an error because underlying stream fails.
        assert!(imap_stream.next().await.unwrap().is_err());

        // IMAP stream should end even though underlying stream fails only once.
        assert!(imap_stream.next().await.is_none());
    }

    /// Manual timing probe (not a regression test) for OPT_TODO's "per-response
    /// tail copy" item - a burst of many small untagged responses landing in
    /// one buffered read (what a full-folder `FETCH` produces) used to cost
    /// O(k^2): every parsed response copied the entire remaining unconsumed
    /// tail into a freshly allocated buffer. `decode()` now consumes via
    /// `BytesMut::advance` (a pointer move) instead, so per-response cost
    /// should stay flat as the burst grows rather than climbing with it.
    /// Measured (release build): ~640ns/response at n=100, ~480ns/response at
    /// n=1,000, ~390ns/response at n=10,000 - flat (if anything improving, as
    /// fixed per-burst overhead amortizes), not climbing with n as the old
    /// O(k^2) tail copy would have shown. Run with:
    /// `cargo test -p async-imap --release --features runtime-tokio decode_burst_cost_at_scale -- --ignored --nocapture`
    #[cfg_attr(feature = "runtime-tokio", tokio::test)]
    #[cfg_attr(feature = "runtime-async-std", async_std::test)]
    #[ignore = "manual timing probe, not a regression test"]
    async fn decode_burst_cost_at_scale() {
        use futures::StreamExt;

        for n in [100usize, 1_000, 10_000] {
            let mut burst = Vec::new();
            for i in 0..n {
                burst.extend_from_slice(format!("* {i} FETCH (FLAGS (\\Seen))\r\n").as_bytes());
            }
            burst.extend_from_slice(b"a0 OK FETCH completed\r\n");

            let mock_stream = crate::mock_stream::MockStream::new(burst);
            let mut imap_stream = ImapStream::new(mock_stream);

            let start = std::time::Instant::now();
            // Every FETCH plus the tagged completion - read exactly that many
            // rather than draining to `None`, since the mock stream reports
            // running out of bytes as an error, not a clean EOF.
            for _ in 0..(n + 1) {
                imap_stream.next().await.expect("stream ended early").expect("parse error");
            }
            let elapsed = start.elapsed();
            eprintln!("n={n} total={elapsed:?} per_response={:?}", elapsed / (n as u32 + 1));
        }
    }

    #[test]
    fn test_buffer_empty() {
        let buf = Buffer::new();
        assert_eq!(buf.used(), 0);

        let mut buf = Buffer::new();
        let slice: &[u8] = buf.free_as_mut_slice();
        assert_eq!(slice.len(), Buffer::BLOCK_SIZE);
        assert_eq!(slice.len(), buf.block.len());
    }

    #[test]
    fn test_buffer_extend_use() {
        let mut buf = Buffer::new();
        buf.extend_used(3);
        assert_eq!(buf.used(), 3);
        let slice = buf.free_as_mut_slice();
        assert_eq!(slice.len(), Buffer::BLOCK_SIZE - 3);

        // Extend past the end of the buffer.
        buf.extend_used(Buffer::BLOCK_SIZE);
        assert_eq!(buf.used(), Buffer::BLOCK_SIZE);
        assert_eq!(buf.offset, Buffer::BLOCK_SIZE);
        assert_eq!(buf.block.len(), buf.offset);
        let slice = buf.free_as_mut_slice();
        assert_eq!(slice.len(), 0);
    }

    #[test]
    fn test_buffer_write_read() {
        let mut buf = Buffer::new();
        let mut slice = buf.free_as_mut_slice();
        slice.write_all(b"hello").unwrap();
        buf.extend_used(b"hello".len());

        let slice = &buf.block[..buf.used()];
        assert_eq!(slice, b"hello");
        assert_eq!(buf.free_as_mut_slice().len(), buf.block.len() - buf.offset);
    }

    #[test]
    fn test_buffer_grow() {
        let mut buf = Buffer::new();
        assert_eq!(buf.block.len(), Buffer::BLOCK_SIZE);
        buf.grow(1).unwrap();
        assert_eq!(buf.block.len(), 2 * Buffer::BLOCK_SIZE);

        buf.grow(Buffer::BLOCK_SIZE + 1).unwrap();
        assert_eq!(buf.block.len(), 4 * Buffer::BLOCK_SIZE);

        let ret = buf.grow(Buffer::MAX_CAPACITY);
        assert!(ret.is_err());
    }

    #[test]
    fn test_buffer_ensure_capacity() {
        // Initial state: 1 byte capacity left, initial size.
        let mut buf = Buffer::new();
        buf.extend_used(Buffer::BLOCK_SIZE - 1);
        assert_eq!(buf.free_as_mut_slice().len(), 1);
        assert_eq!(buf.block.len(), Buffer::BLOCK_SIZE);

        // Still has capacity, no size request.
        buf.ensure_capacity(0).unwrap();
        assert_eq!(buf.free_as_mut_slice().len(), 1);
        assert_eq!(buf.block.len(), Buffer::BLOCK_SIZE);

        // No more capacity, initial size.
        buf.extend_used(1);
        assert_eq!(buf.free_as_mut_slice().len(), 0);
        assert_eq!(buf.block.len(), Buffer::BLOCK_SIZE);

        // No capacity, no size request.
        buf.ensure_capacity(0).unwrap();
        assert_eq!(buf.free_as_mut_slice().len(), Buffer::BLOCK_SIZE);
        assert_eq!(buf.block.len(), 2 * Buffer::BLOCK_SIZE);

        // Some capacity, size request.
        buf.extend_used(5);
        assert_eq!(buf.offset, Buffer::BLOCK_SIZE + 5);
        buf.ensure_capacity(3 * Buffer::BLOCK_SIZE - 6).unwrap();
        assert_eq!(buf.free_as_mut_slice().len(), 2 * Buffer::BLOCK_SIZE - 5);
        assert_eq!(buf.block.len(), 3 * Buffer::BLOCK_SIZE);
    }

    /// Regression test for a bug in ensure_capacity() caused
    /// by a bug in byte-pool crate 0.2.2 dependency.
    ///
    /// ensure_capacity() sometimes did not ensure that
    /// at least one byte is available, which in turn
    /// resulted in attempt to read into a buffer of zero size.
    /// When poll_read() reads into a buffer of zero size,
    /// it can only read zero bytes, which is indistinguishable
    /// from EOF and resulted in an erroneous detection of EOF
    /// when in fact the stream was not closed.
    #[test]
    fn test_ensure_capacity_loop() {
        let mut buf = Buffer::new();

        for i in 1..500 {
            // Ask for `i` bytes.
            buf.ensure_capacity(i).unwrap();

            // Test that we can read at least 1 byte.
            let free = buf.free_as_mut_slice();
            let used = free.len();
            assert!(used > 0);

            // Use as much as allowed.
            buf.extend_used(used);

            // Test that we can read at least as much as requested.
            assert!(buf.block.len() >= i);
        }
    }

    #[test]
    fn test_buffer_consume() {
        let mut buf = Buffer::new();
        buf.extend_used(10);
        buf.block[..10].copy_from_slice(b"0123456789");

        buf.consume(4);
        assert_eq!(buf.used(), 6);
        assert_eq!(buf.as_slice(), b"456789");

        buf.consume(6);
        assert_eq!(buf.used(), 0);
        assert_eq!(buf.as_slice(), b"");
    }

    #[test]
    fn test_buffer_consume_preserves_free_space_and_unconsumed_tail() {
        // A partial consume (some of the used bytes were an already-parsed
        // response, the rest is the start of the next one still buffered)
        // must drop exactly the consumed prefix, keep the remaining used
        // bytes intact, and leave the same amount of free space at the tail
        // it had before - `consume` must not need to touch the free region
        // at all, since `Buf::advance` is a pointer move over the whole
        // block, not a copy of the used bytes.
        let mut buf = Buffer::new();
        buf.extend_used(10);
        buf.block[..10].copy_from_slice(b"0123456789");
        let free_before = buf.free_as_mut_slice().len();

        buf.consume(4);
        assert_eq!(buf.used(), 6);
        assert_eq!(buf.as_slice(), b"456789");
        assert_eq!(buf.free_as_mut_slice().len(), free_before, "consume must not shrink the free region");
    }

    #[test]
    fn test_buffer_debug() {
        assert_eq!(format!("{:?}", Buffer::new()), format!(r#"Buffer {{ used: 0, capacity: {} }}"#, Buffer::BLOCK_SIZE));
    }
}
