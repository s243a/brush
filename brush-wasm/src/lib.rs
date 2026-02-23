pub mod wasm_vfs;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use brush_builtins::ShellBuilderExt;
use brush_builtins::BuiltinSet;
use brush_core::openfiles::{OpenFile, Stream};
use brush_core::{ExecutionParameters, ProfileLoadBehavior, RcLoadBehavior, SourceInfo};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// In-memory buffer that implements brush_core's Stream trait.
/// Used to capture stdout/stderr output from the shell.
#[derive(Clone)]
struct InMemoryStream {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl InMemoryStream {
    fn new() -> Self {
        Self {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn take_contents(&self) -> Vec<u8> {
        let mut buf = self.buf.lock().unwrap();
        std::mem::take(&mut *buf)
    }
}

impl Read for InMemoryStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        // stdout/stderr streams are write-only; reads return EOF.
        Ok(0)
    }
}

impl Write for InMemoryStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.buf.lock().unwrap();
        inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

unsafe impl Send for InMemoryStream {}
unsafe impl Sync for InMemoryStream {}

impl Stream for InMemoryStream {
    fn clone_box(&self) -> Box<dyn Stream> {
        Box::new(self.clone())
    }
}

/// Empty stream for stdin (no interactive input in WASM).
#[derive(Clone)]
struct EmptyInputStream;

impl Read for EmptyInputStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0) // EOF
    }
}

impl Write for EmptyInputStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len()) // discard
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

unsafe impl Send for EmptyInputStream {}
unsafe impl Sync for EmptyInputStream {}

impl Stream for EmptyInputStream {
    fn clone_box(&self) -> Box<dyn Stream> {
        Box::new(self.clone())
    }
}

/// Result of executing a bash script.
#[derive(Serialize)]
struct ExecResult {
    stdout: String,
    stderr: String,
    exit_code: u8,
}

/// Holds the shell instance and its captured output streams.
struct ShellState {
    shell: brush_core::Shell,
    stdout_stream: InMemoryStream,
    stderr_stream: InMemoryStream,
}

/// Opaque handle to a shell instance, stored on the JS side.
#[wasm_bindgen]
pub struct BrushShell {
    state: Arc<Mutex<ShellState>>,
}

#[wasm_bindgen]
impl BrushShell {
    /// Create a new shell instance configured for WASM execution.
    pub async fn create() -> Result<BrushShell, JsError> {
        // Install panic hook for readable WASM stack traces.
        console_error_panic_hook::set_once();

        web_sys::console::log_1(&"[BrushShell] Starting create()...".into());

        // Register SharedVFS hooks for file I/O
        brush_core::set_wasm_vfs(
            |path, read, write, create, append| {
                wasm_vfs::open_file(path, read, write, create, append)
            },
            |path| wasm_vfs::exists(path),
        );

        // Register extended VFS hooks for directory listing, remove, stat, mkdir
        brush_core::set_wasm_vfs_extended(
            |path| wasm_vfs::list_dir(path),
            |path| wasm_vfs::remove(path),
            |path| wasm_vfs::stat(path),
            |path| wasm_vfs::mkdir(path),
        );

        let stdout_stream = InMemoryStream::new();
        let stderr_stream = InMemoryStream::new();

        let mut fds = HashMap::new();
        fds.insert(0, OpenFile::Stream(Box::new(EmptyInputStream)));
        fds.insert(1, OpenFile::Stream(Box::new(stdout_stream.clone())));
        fds.insert(2, OpenFile::Stream(Box::new(stderr_stream.clone())));

        web_sys::console::log_1(&"[BrushShell] Building shell...".into());

        let builder = brush_core::Shell::builder()
            .interactive(false)
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .do_not_inherit_env(true)
            .skip_well_known_vars(false)
            .command_string_mode(true)
            .working_dir(std::path::PathBuf::from("/"))
            .fds(fds)
            .default_builtins(BuiltinSet::BashMode)
            .builtins(brush_uutils::uutils_builtins());

        web_sys::console::log_1(&"[BrushShell] Builder ready, calling build()...".into());

        let shell = builder
            .build()
            .await
            .map_err(|e| JsError::new(&format!("Failed to create shell: {e}")))?;

        web_sys::console::log_1(&"[BrushShell] Shell built successfully!".into());

        Ok(BrushShell {
            state: Arc::new(Mutex::new(ShellState {
                shell,
                stdout_stream,
                stderr_stream,
            })),
        })
    }

    /// Execute a bash script and return {stdout, stderr, exit_code}.
    pub async fn execute(&self, script: &str) -> Result<JsValue, JsError> {
        let state_arc = self.state.clone();
        let script = script.to_string();

        let mut state = state_arc.lock().unwrap();

        // Clear buffers from previous execution.
        state.stdout_stream.take_contents();
        state.stderr_stream.take_contents();

        let params = ExecutionParameters::default();
        let source_info = SourceInfo {
            source: String::from("-c"),
            start: None,
        };

        let result = state
            .shell
            .run_string(&script, &source_info, &params)
            .await
            .map_err(|e| JsError::new(&format!("Execution error: {e}")))?;

        let exit_code: u8 = result.exit_code.into();

        let stdout_bytes = state.stdout_stream.take_contents();
        let stderr_bytes = state.stderr_stream.take_contents();

        let exec_result = ExecResult {
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            exit_code,
        };

        serde_wasm_bindgen::to_value(&exec_result)
            .map_err(|e| JsError::new(&format!("Serialization error: {e}")))
    }
}
