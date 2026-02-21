use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::error;
use crate::openfiles::{OpenFile, Stream};

/// Maximum pipe buffer size (64 MB). On WASM, process substitution runs the
/// writer to completion before the reader starts (sync Read can't yield), so the
/// entire output accumulates in memory. This cap prevents OOM for runaway writers.
/// For large data, use pipelines (`cmd1 | cmd2`) which stream sequentially.
const MAX_PIPE_BUFFER_BYTES: usize = 64 * 1024 * 1024;

/// Shared buffer between pipe reader and writer.
struct SharedBuffer {
    data: Vec<u8>,
    read_pos: usize,
    writer_closed: bool,
    /// Waker to notify the async reader when data arrives or writer closes.
    reader_waker: Option<Waker>,
}

/// Atomic counter for live writer instances. The pipe is only closed (EOF)
/// when ALL writer clones have been dropped, matching OS pipe semantics.
struct WriterCount(AtomicUsize);

/// In-memory pipe reader that reads from a shared buffer.
pub(crate) struct InMemoryPipeReader {
    buffer: Arc<Mutex<SharedBuffer>>,
}

/// In-memory pipe writer that writes to a shared buffer.
pub(crate) struct InMemoryPipeWriter {
    buffer: Arc<Mutex<SharedBuffer>>,
    writer_count: Arc<WriterCount>,
}

impl Read for InMemoryPipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut shared = self.buffer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        let available = shared.data.len() - shared.read_pos;
        if available == 0 {
            // No data available; return EOF (0 bytes).
            // On WASM (single-threaded), if the writer hasn't closed yet,
            // more data can't arrive until we yield, so returning 0 is correct.
            return Ok(0);
        }
        let to_read = buf.len().min(available);
        buf[..to_read]
            .copy_from_slice(&shared.data[shared.read_pos..shared.read_pos + to_read]);
        shared.read_pos += to_read;
        // Compact buffer when we've consumed more than half
        let half = shared.data.len() / 2;
        if shared.read_pos > half && shared.read_pos > 0 {
            let pos = shared.read_pos;
            shared.data.drain(..pos);
            shared.read_pos = 0;
        }
        Ok(to_read)
    }
}

impl Write for InMemoryPipeReader {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("pipe reader is not writable"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tokio::io::AsyncRead for InMemoryPipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut shared = self.buffer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;

        let available = shared.data.len() - shared.read_pos;
        if available > 0 {
            let to_read = buf.remaining().min(available);
            buf.put_slice(&shared.data[shared.read_pos..shared.read_pos + to_read]);
            shared.read_pos += to_read;
            // Compact buffer when we've consumed more than half
            let half = shared.data.len() / 2;
            if shared.read_pos > half && shared.read_pos > 0 {
                let pos = shared.read_pos;
                shared.data.drain(..pos);
                shared.read_pos = 0;
            }
            Poll::Ready(Ok(()))
        } else if shared.writer_closed {
            // Real EOF — writer is done, no more data coming.
            Poll::Ready(Ok(()))
        } else {
            // No data yet, writer still open — register waker and yield.
            shared.reader_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Stream for InMemoryPipeReader {
    fn clone_box(&self) -> Box<dyn Stream> {
        Box::new(InMemoryPipeReader {
            buffer: Arc::clone(&self.buffer),
        })
    }

    #[cfg(unix)]
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error> {
        Err(error::Error::from(error::ErrorKind::Unimplemented(
            "pipe reader cannot provide OwnedFd",
        )))
    }

    #[cfg(unix)]
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
        Err(error::Error::from(error::ErrorKind::Unimplemented(
            "pipe reader cannot provide BorrowedFd",
        )))
    }
}

impl Read for InMemoryPipeWriter {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("pipe writer is not readable"))
    }
}

impl Write for InMemoryPipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut shared = self.buffer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        if shared.writer_closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pipe closed",
            ));
        }
        // Enforce buffer size limit to prevent OOM on WASM where process
        // substitution must buffer the entire writer output before the reader starts.
        let new_size = shared.data.len() + buf.len();
        if new_size > MAX_PIPE_BUFFER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                std::format!(
                    "pipe buffer exceeded {:.0} MB limit; use pipelines for large data",
                    MAX_PIPE_BUFFER_BYTES as f64 / (1024.0 * 1024.0)
                ),
            ));
        }
        shared.data.extend_from_slice(buf);
        // Wake the async reader if it's waiting for data.
        if let Some(waker) = shared.reader_waker.take() {
            waker.wake();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stream for InMemoryPipeWriter {
    fn clone_box(&self) -> Box<dyn Stream> {
        // Increment writer count — the pipe stays open until ALL writers are dropped.
        self.writer_count.0.fetch_add(1, Ordering::Relaxed);
        Box::new(InMemoryPipeWriter {
            buffer: Arc::clone(&self.buffer),
            writer_count: Arc::clone(&self.writer_count),
        })
    }

    #[cfg(unix)]
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, error::Error> {
        Err(error::Error::from(error::ErrorKind::Unimplemented(
            "pipe writer cannot provide OwnedFd",
        )))
    }

    #[cfg(unix)]
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, error::Error> {
        Err(error::Error::from(error::ErrorKind::Unimplemented(
            "pipe writer cannot provide BorrowedFd",
        )))
    }
}

impl Drop for InMemoryPipeWriter {
    fn drop(&mut self) {
        // Only mark the pipe as closed when the LAST writer is dropped,
        // matching OS pipe semantics where the pipe stays open as long as
        // any file descriptor referencing the write end exists.
        let prev = self.writer_count.0.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            // This was the last writer.
            if let Ok(mut shared) = self.buffer.lock() {
                shared.writer_closed = true;
                // Wake the async reader so it sees EOF.
                if let Some(waker) = shared.reader_waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

/// Creates a new in-memory pipe, returning (reader, writer) as OpenFile pairs.
pub(crate) fn pipe() -> std::io::Result<(OpenFile, OpenFile)> {
    let shared = Arc::new(Mutex::new(SharedBuffer {
        data: Vec::new(),
        read_pos: 0,
        writer_closed: false,
        reader_waker: None,
    }));
    let writer_count = Arc::new(WriterCount(AtomicUsize::new(1)));
    let reader = InMemoryPipeReader {
        buffer: Arc::clone(&shared),
    };
    let writer = InMemoryPipeWriter {
        buffer: shared,
        writer_count,
    };
    Ok((
        OpenFile::Stream(Box::new(reader)),
        OpenFile::Stream(Box::new(writer)),
    ))
}
