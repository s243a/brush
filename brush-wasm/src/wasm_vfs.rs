//! WASM Virtual Filesystem — bridges brush shell file I/O to JavaScript SharedVFS.
//!
//! Uses wasm-bindgen to import JS functions from `window.sharedVFS`, providing
//! file read/write capabilities that brush-core's `open_file()` delegates to on WASM.
//!
//! Small files (< 1MB) are buffered entirely in Rust Vec<u8>.
//! Large files spill to JS-side storage and are accessed in chunks.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use brush_core::openfiles::{OpenFile, Stream};
use wasm_bindgen::prelude::*;

/// Threshold for spilling large files to JS-side storage.
const SPILL_THRESHOLD: usize = 1_048_576; // 1MB

// ── JS imports from window.sharedVFS ────────────────────────────

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_read_file")]
    fn js_vfs_read_file(path: &str) -> JsValue;

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_write_file")]
    fn js_vfs_write_file(path: &str, content: &[u8]);

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_exists")]
    fn js_vfs_exists(path: &str) -> bool;

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_mkdir")]
    fn js_vfs_mkdir(path: &str);

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_read_chunk")]
    fn js_vfs_read_chunk(path: &str, offset: u32, len: u32) -> JsValue;

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_append_chunk")]
    fn js_vfs_append_chunk(path: &str, chunk: &[u8]);

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_list_dir")]
    fn js_vfs_list_dir(path: &str) -> JsValue;

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_remove")]
    fn js_vfs_remove(path: &str) -> bool;

    #[wasm_bindgen(js_namespace = ["window", "sharedVFS"], js_name = "vfs_stat")]
    fn js_vfs_stat(path: &str) -> JsValue;
}

// ── Helper: convert JsValue (Uint8Array or null) to Option<Vec<u8>> ──

fn js_to_bytes(val: JsValue) -> Option<Vec<u8>> {
    if val.is_null() || val.is_undefined() {
        return None;
    }
    js_sys::Uint8Array::new(&val)
        .to_vec()
        .into()
}

// ── JsBackedFile ────────────────────────────────────────────────

/// A file backed by JavaScript's SharedVFS.
///
/// Small files are buffered in a Rust `Vec<u8>`. When a file exceeds
/// `SPILL_THRESHOLD`, it is flushed to JS and subsequent operations
/// use chunked access.
struct JsBackedFileInner {
    path: String,
    buffer: Vec<u8>,
    position: usize,
    writable: bool,
    append: bool,
    dirty: bool,
    spilled: bool,
    /// Total size of file in JS (only valid when spilled)
    js_size: usize,
}

/// Thread-safe wrapper (WASM is single-threaded, but brush-core requires Send+Sync).
#[derive(Clone)]
pub struct JsBackedFile {
    inner: Arc<Mutex<JsBackedFileInner>>,
}

impl JsBackedFile {
    /// Open a file for reading. Loads content from SharedVFS.
    fn open_read(path: &str) -> io::Result<Self> {
        let val = js_vfs_read_file(path);
        let buffer = js_to_bytes(val).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("file not found: {path}"))
        })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(JsBackedFileInner {
                path: path.to_string(),
                buffer,
                position: 0,
                writable: false,
                append: false,
                dirty: false,
                spilled: false,
                js_size: 0,
            })),
        })
    }

    /// Open a file for writing (truncate).
    fn open_write(path: &str) -> Self {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(path).parent() {
            js_vfs_mkdir(&parent.to_string_lossy());
        }
        Self {
            inner: Arc::new(Mutex::new(JsBackedFileInner {
                path: path.to_string(),
                buffer: Vec::new(),
                position: 0,
                writable: true,
                append: false,
                dirty: true,
                spilled: false,
                js_size: 0,
            })),
        }
    }

    /// Open a file for appending.
    fn open_append(path: &str) -> Self {
        // Load existing content if any
        let val = js_vfs_read_file(path);
        let buffer = js_to_bytes(val).unwrap_or_default();
        let position = buffer.len();

        if let Some(parent) = Path::new(path).parent() {
            js_vfs_mkdir(&parent.to_string_lossy());
        }

        Self {
            inner: Arc::new(Mutex::new(JsBackedFileInner {
                path: path.to_string(),
                buffer,
                position,
                writable: true,
                append: true,
                dirty: true,
                spilled: false,
                js_size: 0,
            })),
        }
    }

    /// Flush buffer to JS SharedVFS.
    fn flush_to_js(inner: &mut JsBackedFileInner) {
        if inner.dirty && !inner.buffer.is_empty() {
            js_vfs_write_file(&inner.path, &inner.buffer);
            inner.dirty = false;
        } else if inner.dirty {
            // Empty write (truncation)
            js_vfs_write_file(&inner.path, &[]);
            inner.dirty = false;
        }
    }

    /// Check if buffer should spill to JS.
    fn maybe_spill(inner: &mut JsBackedFileInner) {
        if !inner.spilled && inner.buffer.len() >= SPILL_THRESHOLD {
            // Flush current buffer to JS
            js_vfs_write_file(&inner.path, &inner.buffer);
            inner.js_size = inner.buffer.len();
            inner.buffer.clear();
            inner.spilled = true;
            inner.dirty = false;
        }
    }
}

impl Read for JsBackedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();

        if inner.spilled {
            // Read from JS in chunks
            let val = js_vfs_read_chunk(
                &inner.path,
                inner.position as u32,
                buf.len() as u32,
            );
            if let Some(bytes) = js_to_bytes(val) {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                inner.position += n;
                Ok(n)
            } else {
                Ok(0) // EOF
            }
        } else {
            // Read from in-memory buffer
            let remaining = &inner.buffer[inner.position..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            inner.position += n;
            Ok(n)
        }
    }
}

impl Write for JsBackedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap();

        if !inner.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only file"));
        }

        if inner.spilled {
            // Append chunk to JS
            js_vfs_append_chunk(&inner.path, buf);
            inner.js_size += buf.len();
        } else {
            inner.buffer.extend_from_slice(buf);
            inner.dirty = true;
            JsBackedFile::maybe_spill(&mut inner);
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.spilled {
            JsBackedFile::flush_to_js(&mut inner);
        }
        Ok(())
    }
}

impl Drop for JsBackedFileInner {
    fn drop(&mut self) {
        // Flush any buffered writes on close
        if self.dirty && self.writable && !self.spilled {
            JsBackedFile::flush_to_js(self);
        }
    }
}

unsafe impl Send for JsBackedFile {}
unsafe impl Sync for JsBackedFile {}

impl Stream for JsBackedFile {
    fn clone_box(&self) -> Box<dyn Stream> {
        Box::new(self.clone())
    }
}

// ── Public API for brush-core's open_file() ─────────────────────

/// Open a file via SharedVFS. Called from brush-core's `open_file()` on WASM.
///
/// Returns `OpenFile::Stream(...)` wrapping a `JsBackedFile`.
pub fn open_file(
    path: &Path,
    read: bool,
    write: bool,
    create: bool,
    append: bool,
) -> io::Result<OpenFile> {
    let path_str = path.to_string_lossy();

    if append {
        Ok(OpenFile::Stream(Box::new(JsBackedFile::open_append(&path_str))))
    } else if write || create {
        Ok(OpenFile::Stream(Box::new(JsBackedFile::open_write(&path_str))))
    } else if read {
        Ok(OpenFile::Stream(Box::new(JsBackedFile::open_read(&path_str)?)))
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "no file mode specified"))
    }
}

/// Check if a path exists in SharedVFS.
pub fn exists(path: &Path) -> bool {
    js_vfs_exists(&path.to_string_lossy())
}

/// Create a directory in SharedVFS.
pub fn mkdir(path: &Path) {
    js_vfs_mkdir(&path.to_string_lossy());
}

/// List directory contents from SharedVFS.
/// Returns Vec of (name, size, is_dir, modified_ms) tuples.
pub fn list_dir(path: &Path) -> Option<Vec<(String, u64, bool, u64)>> {
    let val = js_vfs_list_dir(&path.to_string_lossy());
    if val.is_null() || val.is_undefined() {
        return None;
    }
    let json_str: String = val.as_string()?;
    // Parse JSON: [{name, size, is_dir, modified}, ...]
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_str).ok()?;
    let entries = parsed
        .into_iter()
        .filter_map(|obj| {
            let name = obj.get("name")?.as_str()?.to_string();
            let size = obj.get("size")?.as_u64().unwrap_or(0);
            let is_dir = obj.get("is_dir")?.as_bool().unwrap_or(false);
            let modified = obj.get("modified")?.as_u64().unwrap_or(0);
            Some((name, size, is_dir, modified))
        })
        .collect();
    Some(entries)
}

/// Remove a file or empty directory from SharedVFS.
pub fn remove(path: &Path) -> bool {
    js_vfs_remove(&path.to_string_lossy())
}

/// Get file/directory stat from SharedVFS.
/// Returns (size, is_dir, modified_ms) or None.
pub fn stat(path: &Path) -> Option<(u64, bool, u64)> {
    let val = js_vfs_stat(&path.to_string_lossy());
    if val.is_null() || val.is_undefined() {
        return None;
    }
    let json_str: String = val.as_string()?;
    let obj: serde_json::Value = serde_json::from_str(&json_str).ok()?;
    let size = obj.get("size")?.as_u64().unwrap_or(0);
    let is_dir = obj.get("is_dir")?.as_bool().unwrap_or(false);
    let modified = obj.get("modified")?.as_u64().unwrap_or(0);
    Some((size, is_dir, modified))
}
