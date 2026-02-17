# brush-wasm

WebAssembly bindings for the [brush](https://github.com/reubeno/brush) shell interpreter. Provides a Bash-compatible shell that runs entirely in the browser, with `declare -A` associative arrays, all standard bash builtins, and stdout/stderr capture.

Built for [SciREPL](https://github.com/nickmilo/sci-repl) / UnifyWeaver integration, where Prolog-generated Bash scripts need to execute client-side.

## Build

Requires [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/) and the `wasm32-unknown-unknown` Rust target.

```bash
# Add WASM target (one-time)
rustup target add wasm32-unknown-unknown

# Build (from the brush workspace root)
wasm-pack build brush-wasm --target web --release --no-opt
```

The `--no-opt` flag skips `wasm-opt` post-processing. The bundled `wasm-opt` in wasm-pack 0.14 doesn't support the bulk memory operations that Rust 1.88+ emits. Install a newer [binaryen](https://github.com/WebAssembly/binaryen) to enable optimization and reduce binary size.

Output goes to `brush-wasm/pkg/`:

| File | Description |
|---|---|
| `brush_wasm_bg.wasm` | WASM binary (~4.3 MB release, unoptimized) |
| `brush_wasm.js` | JS glue code with ES module exports |
| `brush_wasm.d.ts` | TypeScript declarations |
| `package.json` | npm package metadata |

## JavaScript API

```javascript
import init, { BrushShell } from './pkg/brush_wasm.js';

// Initialize the WASM module (load .wasm file)
await init();

// Create a shell instance
const shell = await BrushShell.create();

// Execute a bash script
const result = await shell.execute(`
  declare -A scores
  scores[alice]=95
  scores[bob]=87
  for name in "\${!scores[@]}"; do
    echo "\$name: \${scores[\$name]}"
  done
`);

console.log(result.stdout);    // "alice: 95\nbob: 87\n"
console.log(result.stderr);    // ""
console.log(result.exit_code); // 0
```

### `BrushShell.create(): Promise<BrushShell>`

Creates a new shell instance configured for WASM:
- Non-interactive mode
- No profile/rc loading
- No inherited environment variables
- All bash builtins registered (declare, read, echo, printf, test, etc.)
- stdout/stderr captured to in-memory buffers

### `shell.execute(script: string): Promise<{stdout, stderr, exit_code}>`

Runs a bash script string and returns:
- `stdout` (string) — captured standard output
- `stderr` (string) — captured standard error
- `exit_code` (number) — 0 for success

The shell instance is persistent across calls — variables, functions, and state from previous `execute()` calls are retained.

### `shell.free()`

Releases the WASM memory for this shell instance. Called automatically if using `Symbol.dispose`.

## Architecture

```
brush-wasm (this crate)
    │
    ├── InMemoryStream     implements brush_core::openfiles::Stream
    │   └── Arc<Mutex<Vec<u8>>> captures stdout/stderr writes
    │
    ├── EmptyInputStream   returns EOF for stdin (no interactive input)
    │
    └── BrushShell         #[wasm_bindgen] exported class
        ├── create()       builds Shell with custom fds + bash builtins
        └── execute()      runs script via shell.run_string(), returns captured output
```

Dependencies:
- `brush-core` — shell interpreter engine
- `brush-builtins` — bash builtin commands (declare, read, echo, etc.)
- `wasm-bindgen` + `wasm-bindgen-futures` — JS interop and async support
- `serde` + `serde-wasm-bindgen` — result serialization to JS objects
- `tokio` (single-threaded rt) — async runtime for brush-core

## Supported Bash Features

Everything brush supports works here. Key features for UnifyWeaver:

| Feature | Example | Status |
|---|---|---|
| Associative arrays | `declare -A map; map[key]=val` | Works |
| Indexed arrays | `declare -a arr=(1 2 3)` | Works |
| Key iteration | `${!map[@]}` | Works |
| Arithmetic | `$(( x + 1 ))` | Works |
| Functions | `fn() { ... }; fn` | Works |
| Here-strings | `read x <<< "hello"` | Works |
| Conditionals | `[[ $x == "y" ]]` | Works |
| Pipes (builtins) | `echo foo | read x` | Works |
| `set -euo pipefail` | Error handling | Works |
| External commands | `cat`, `sort`, `grep` | Not yet (no dispatch) |
| File I/O | `> file.txt` | Not yet (no VFS) |

## Limitations

- **No external commands**: Commands like `cat`, `sort`, `grep` require either linking uutils/coreutils or dispatching via JS callbacks. Currently, only bash builtins work.
- **No filesystem**: File redirections (`>`, `<`) go to stub I/O. A VFS adapter is needed for file operations.
- **No interactive input**: stdin returns EOF immediately. This is by design for script execution.
- **Single-threaded**: Runs on tokio's current-thread runtime. No background jobs or parallel pipelines.
- **No wasm-opt**: The bundled wasm-opt doesn't support bulk memory operations. Use `--no-opt` or install binaryen >= 116.

## Next Steps

1. **VFS integration** — Wire file I/O to SciREPL's PrologVFS via a JS adapter
2. **External commands** — Link uutils/coreutils or dispatch via JS callbacks
3. **SciREPL kernel** — Add as a "Bash" kernel option in the notebook UI
4. **Prolog bridge** — Enable `js_call/1` from Prolog to invoke `shell.execute()`
5. **Binary size** — Optimize with newer wasm-opt; strip unused builtins
