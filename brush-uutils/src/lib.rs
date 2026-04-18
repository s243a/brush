//! Bridge crate: exposes uutils/coreutils commands as brush shell builtins.
//!
//! Each utility is wrapped with [`uucore::wasm_io::with_wasm_io`] so that
//! stdout/stderr/stdin are redirected through the brush shell's file
//! descriptors instead of being silently discarded on `wasm32-unknown-unknown`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::BufReader;

use brush_core::builtins::{self, ContentOptions, ContentType, SimpleCommand};
use brush_core::{commands, error, extensions, results};

use std::io::{Read, Write as IoWrite};

/// Macro that generates a `SimpleCommand` wrapper for a uutils utility.
///
/// Each generated struct:
/// - Installs the brush context's stdin/stdout/stderr as wasm_io thread-local overrides
/// - Calls the utility's `uumain(args)` function
/// - Converts the i32 exit code to an `ExecutionResult`
macro_rules! uutils_builtin {
    ($struct_name:ident, $cmd_name:literal, $crate_path:path) => {
        /// Auto-generated builtin wrapper for the `
        #[doc = $cmd_name]
        /// ` command.
        pub struct $struct_name;

        impl SimpleCommand for $struct_name {
            fn get_content(
                name: &str,
                _content_type: ContentType,
                _options: &ContentOptions,
            ) -> Result<String, error::Error> {
                Ok(format!("{name}: uutils coreutils command\n"))
            }

            fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
                context: commands::ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<results::ExecutionResult, error::Error> {
                // brush passes the command name as args[0], so we don't prepend it.
                let os_args: Vec<OsString> = args.map(|a| OsString::from(a.as_ref())).collect();

                #[cfg(target_family = "wasm")]
                let exit_code = {
                    // Install VFS file hooks so builtins can open files by path on WASM.
                    uucore::wasm_io::set_file_hooks(
                        Box::new(|path| brush_core::wasm_open_file_for_read(path)),
                        Box::new(|path| brush_core::wasm_file_exists(path)),
                    );

                    uucore::wasm_io::with_wasm_io(
                        Box::new(context.stdin()),
                        Box::new(context.stdout()),
                        Box::new(context.stderr()),
                        || {
                            use $crate_path as uu;
                            uu::uumain(os_args.into_iter())
                        },
                    )
                };

                #[cfg(not(target_family = "wasm"))]
                let exit_code = {
                    let _ = &context; // suppress unused warning
                    use $crate_path as uu;
                    uu::uumain(os_args.into_iter())
                };

                #[expect(clippy::cast_sign_loss)]
                Ok(results::ExecutionResult::new((exit_code & 0xFF) as u8))
            }
        }
    };
}

// Generate builtin wrappers for Tier 1 utilities.
uutils_builtin!(CatBuiltin, "cat", uu_cat);
uutils_builtin!(EchoBuiltin, "echo", uu_echo);
uutils_builtin!(SortBuiltin, "sort", uu_sort);
uutils_builtin!(UniqBuiltin, "uniq", uu_uniq);
uutils_builtin!(WcBuiltin, "wc", uu_wc);
uutils_builtin!(HeadBuiltin, "head", uu_head);
uutils_builtin!(TailBuiltin, "tail", uu_tail);
uutils_builtin!(CutBuiltin, "cut", uu_cut);
uutils_builtin!(TrBuiltin, "tr", uu_tr);
uutils_builtin!(SeqBuiltin, "seq", uu_seq);
uutils_builtin!(PrintfBuiltin, "printf", uu_printf);

/// Builtin wrapper for `grep` (ripgrep library-based implementation).
pub struct GrepBuiltin;

impl SimpleCommand for GrepBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: grep (POSIX-compatible, ripgrep engine)\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        // brush passes the command name as args[0], so we don't prepend it.
        let full_args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();

        let mut stdin_reader = BufReader::new(context.stdin());
        let mut stdout_writer = context.stdout();
        let mut stderr_writer = context.stderr();

        let exit_code = grep_wasm::grep_main(
            full_args.iter().map(String::as_str),
            &mut stdin_reader,
            &mut stdout_writer,
            &mut stderr_writer,
        );

        #[expect(clippy::cast_sign_loss)]
        Ok(results::ExecutionResult::new((exit_code & 0xFF) as u8))
    }
}

// ── Custom builtins (VFS-aware on WASM) ──────────────────────────

/// `ls` — list directory contents.
/// On WASM, uses VFS hooks. On native, uses std::fs.
pub struct LsBuiltin;

impl SimpleCommand for LsBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: list directory contents\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stdout = context.stdout();
        let mut stderr = context.stderr();

        // Parse flags
        let mut long_format = false;
        let mut show_hidden = false;
        let mut one_per_line = false;
        let mut human_sizes = false;
        let mut paths: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg.starts_with('-') && arg.len() > 1 && !arg.starts_with("--") {
                for ch in arg[1..].chars() {
                    match ch {
                        'l' => long_format = true,
                        'a' => show_hidden = true,
                        '1' => one_per_line = true,
                        'h' => human_sizes = true,
                        _ => {
                            let _ = writeln!(stderr, "ls: invalid option -- '{ch}'");
                            return Ok(results::ExecutionResult::new(2));
                        }
                    }
                }
            } else if arg != "ls" || !paths.is_empty() {
                paths.push(arg.clone());
            }
        }

        if paths.is_empty() {
            paths.push(".".to_string());
        }

        let shell = context.shell;
        let multi = paths.len() > 1;

        for (i, path_str) in paths.iter().enumerate() {
            let path = shell.absolute_path(std::path::Path::new(path_str));

            if multi {
                if i > 0 {
                    let _ = writeln!(stdout);
                }
                let _ = writeln!(stdout, "{path_str}:");
            }

            #[cfg(target_family = "wasm")]
            {
                if let Some(entries) = brush_core::wasm_list_dir(&path) {
                    for (name, size, is_dir, _modified) in &entries {
                        if !show_hidden && name.starts_with('.') {
                            continue;
                        }
                        if long_format {
                            let type_char = if *is_dir { 'd' } else { '-' };
                            let size_str = if human_sizes {
                                human_readable_size(*size)
                            } else {
                                format!("{size:>8}")
                            };
                            let _ = writeln!(stdout, "{type_char}rw-r--r-- 1 user user {size_str} {name}");
                        } else if one_per_line {
                            let _ = writeln!(stdout, "{name}");
                        } else {
                            let _ = write!(stdout, "{name}  ");
                        }
                    }
                    if !long_format && !one_per_line {
                        let _ = writeln!(stdout);
                    }
                } else if let Some((size, is_dir, _modified)) = brush_core::wasm_stat(&path) {
                    // Not a directory listing — single file (or directory without contents).
                    // Treat as a one-entry listing by its display name.
                    let name = std::path::Path::new(path_str)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path_str.clone());
                    if long_format {
                        let type_char = if is_dir { 'd' } else { '-' };
                        let size_str = if human_sizes {
                            human_readable_size(size)
                        } else {
                            format!("{size:>8}")
                        };
                        let _ = writeln!(stdout, "{type_char}rw-r--r-- 1 user user {size_str} {name}");
                    } else if one_per_line {
                        let _ = writeln!(stdout, "{name}");
                    } else {
                        let _ = writeln!(stdout, "{name}");
                    }
                } else {
                    let _ = writeln!(stderr, "ls: cannot access '{path_str}': No such file or directory");
                    return Ok(results::ExecutionResult::new(2));
                }
            }

            #[cfg(not(target_family = "wasm"))]
            {
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
                        entries.sort_by_key(|e| e.file_name());
                        for entry in &entries {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if !show_hidden && name.starts_with('.') {
                                continue;
                            }
                            if long_format {
                                let meta = entry.metadata().ok();
                                let is_dir = meta.as_ref().map_or(false, |m| m.is_dir());
                                let size = meta.as_ref().map_or(0, |m| m.len());
                                let type_char = if is_dir { 'd' } else { '-' };
                                let size_str = if human_sizes {
                                    human_readable_size(size)
                                } else {
                                    format!("{size:>8}")
                                };
                                let _ = writeln!(stdout, "{type_char}rw-r--r-- 1 user user {size_str} {name}");
                            } else if one_per_line {
                                let _ = writeln!(stdout, "{name}");
                            } else {
                                let _ = write!(stdout, "{name}  ");
                            }
                        }
                        if !long_format && !one_per_line {
                            let _ = writeln!(stdout);
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(stderr, "ls: cannot access '{path_str}': {e}");
                        return Ok(results::ExecutionResult::new(2));
                    }
                }
            }
        }

        Ok(results::ExecutionResult::new(0))
    }
}

fn human_readable_size(size: u64) -> String {
    if size < 1024 {
        format!("{size:>4}")
    } else if size < 1024 * 1024 {
        format!("{:>4.1}K", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:>4.1}M", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:>4.1}G", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// `mkdir` — create directories.
pub struct MkdirBuiltin;

impl SimpleCommand for MkdirBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: create directories\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stderr = context.stderr();
        let mut parents = false;
        let mut dirs: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg == "-p" || arg == "--parents" {
                parents = true;
            } else if arg.starts_with('-') {
                let _ = writeln!(stderr, "mkdir: unrecognized option '{arg}'");
                return Ok(results::ExecutionResult::new(1));
            } else {
                dirs.push(arg.clone());
            }
        }

        if dirs.is_empty() {
            let _ = writeln!(stderr, "mkdir: missing operand");
            return Ok(results::ExecutionResult::new(1));
        }

        let shell = context.shell;
        #[allow(unused_mut)]
        let mut exit_code = 0u8;

        for dir_str in &dirs {
            let path = shell.absolute_path(std::path::Path::new(dir_str));

            #[cfg(target_family = "wasm")]
            {
                brush_core::wasm_mkdir(&path);
            }

            #[cfg(not(target_family = "wasm"))]
            {
                let result = if parents {
                    std::fs::create_dir_all(&path)
                } else {
                    std::fs::create_dir(&path)
                };
                if let Err(e) = result {
                    let _ = writeln!(stderr, "mkdir: cannot create directory '{dir_str}': {e}");
                    exit_code = 1;
                }
            }

            let _ = parents; // suppress unused warning on WASM
        }

        Ok(results::ExecutionResult::new(exit_code))
    }
}

/// `rm` — remove files or directories.
pub struct RmBuiltin;

impl SimpleCommand for RmBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: remove files or directories\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stderr = context.stderr();
        let mut force = false;
        let mut files: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg.starts_with('-') && arg.len() > 1 {
                for ch in arg[1..].chars() {
                    match ch {
                        'f' => force = true,
                        'r' | 'R' => {} // recursive — VFS remove handles this differently
                        _ => {
                            let _ = writeln!(stderr, "rm: invalid option -- '{ch}'");
                            return Ok(results::ExecutionResult::new(1));
                        }
                    }
                }
            } else {
                files.push(arg.clone());
            }
        }

        if files.is_empty() {
            if !force {
                let _ = writeln!(stderr, "rm: missing operand");
                return Ok(results::ExecutionResult::new(1));
            }
            return Ok(results::ExecutionResult::new(0));
        }

        let shell = context.shell;
        let mut exit_code = 0u8;

        for file_str in &files {
            let path = shell.absolute_path(std::path::Path::new(file_str));

            #[cfg(target_family = "wasm")]
            {
                if !brush_core::wasm_remove(&path) && !force {
                    let _ = writeln!(stderr, "rm: cannot remove '{file_str}': No such file or directory");
                    exit_code = 1;
                }
            }

            #[cfg(not(target_family = "wasm"))]
            {
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(e) = result {
                    if !force {
                        let _ = writeln!(stderr, "rm: cannot remove '{file_str}': {e}");
                        exit_code = 1;
                    }
                }
            }
        }

        Ok(results::ExecutionResult::new(exit_code))
    }
}

/// `mv` — move (rename) files.
pub struct MvBuiltin;

impl SimpleCommand for MvBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: move (rename) files\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stderr = context.stderr();
        let mut positional: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg.starts_with('-') && arg.len() > 1 {
                // Ignore flags like -f, -n, -v for now
            } else {
                positional.push(arg.clone());
            }
        }

        if positional.len() != 2 {
            let _ = writeln!(stderr, "mv: expected 2 arguments, got {}", positional.len());
            return Ok(results::ExecutionResult::new(1));
        }

        let shell = context.shell;
        let src = shell.absolute_path(std::path::Path::new(&positional[0]));
        let dst = shell.absolute_path(std::path::Path::new(&positional[1]));

        #[cfg(target_family = "wasm")]
        {
            // Read source content via VFS.
            let content = match brush_core::wasm_open_file_for_read(&src) {
                Ok(mut reader) => {
                    let mut buf = Vec::new();
                    if let Err(e) = std::io::Read::read_to_end(&mut reader, &mut buf) {
                        let _ = writeln!(stderr, "mv: cannot read '{}': {e}", positional[0]);
                        return Ok(results::ExecutionResult::new(1));
                    }
                    buf
                }
                Err(e) => {
                    let _ = writeln!(stderr, "mv: cannot stat '{}': {e}", positional[0]);
                    return Ok(results::ExecutionResult::new(1));
                }
            };

            // Write to destination.
            match brush_core::wasm_open_file(&dst, false, true, true, false) {
                Ok(mut writer) => {
                    if let Err(e) = std::io::Write::write_all(&mut writer, &content) {
                        let _ = writeln!(stderr, "mv: cannot write '{}': {e}", positional[1]);
                        return Ok(results::ExecutionResult::new(1));
                    }
                }
                Err(e) => {
                    let _ = writeln!(stderr, "mv: cannot create '{}': {e}", positional[1]);
                    return Ok(results::ExecutionResult::new(1));
                }
            }

            // Remove source.
            brush_core::wasm_remove(&src);
        }

        #[cfg(not(target_family = "wasm"))]
        {
            if let Err(e) = std::fs::rename(&src, &dst) {
                let _ = writeln!(stderr, "mv: cannot move '{}' to '{}': {e}", positional[0], positional[1]);
                return Ok(results::ExecutionResult::new(1));
            }
        }

        Ok(results::ExecutionResult::new(0))
    }
}

/// `touch` — create empty files or update timestamps.
pub struct TouchBuiltin;

impl SimpleCommand for TouchBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: create empty files\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stderr = context.stderr();
        let mut files: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg.starts_with('-') {
                // Ignore flags like -c (don't create), -m (modify only), etc.
                continue;
            }
            files.push(arg.clone());
        }

        if files.is_empty() {
            let _ = writeln!(stderr, "touch: missing file operand");
            return Ok(results::ExecutionResult::new(1));
        }

        let shell = context.shell;

        for file_str in &files {
            let path = shell.absolute_path(std::path::Path::new(file_str));

            #[cfg(target_family = "wasm")]
            {
                if !brush_core::wasm_file_exists(&path) {
                    match brush_core::wasm_open_file(&path, false, true, true, false) {
                        Ok(_) => {} // File created
                        Err(e) => {
                            let _ = writeln!(stderr, "touch: cannot touch '{file_str}': {e}");
                        }
                    }
                }
            }

            #[cfg(not(target_family = "wasm"))]
            {
                if !path.exists() {
                    if let Err(e) = std::fs::File::create(&path) {
                        let _ = writeln!(stderr, "touch: cannot touch '{file_str}': {e}");
                    }
                }
            }
        }

        Ok(results::ExecutionResult::new(0))
    }
}

/// `tee` — read stdin and write to stdout and files.
pub struct TeeBuiltin;

impl SimpleCommand for TeeBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: read from stdin, write to stdout and files\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stderr = context.stderr();
        let mut append = false;
        let mut file_paths: Vec<String> = Vec::new();

        for arg in args.iter().skip(1) {
            if arg == "-a" || arg == "--append" {
                append = true;
            } else if arg.starts_with('-') && arg.len() > 1 {
                // Ignore other flags
            } else {
                file_paths.push(arg.clone());
            }
        }

        // Read all stdin
        let mut input = Vec::new();
        let mut stdin = context.stdin();
        let _ = stdin.read_to_end(&mut input);

        // Write to stdout
        let mut stdout = context.stdout();
        let _ = stdout.write_all(&input);

        // Write to each file
        let shell = context.shell;
        for file_str in &file_paths {
            let path = shell.absolute_path(std::path::Path::new(file_str));

            #[cfg(target_family = "wasm")]
            {
                match brush_core::wasm_open_file(&path, false, true, true, append) {
                    Ok(mut f) => {
                        let _ = f.write_all(&input);
                    }
                    Err(e) => {
                        let _ = writeln!(stderr, "tee: '{file_str}': {e}");
                    }
                }
            }

            #[cfg(not(target_family = "wasm"))]
            {
                let result = if append {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                } else {
                    std::fs::File::create(&path)
                };
                match result {
                    Ok(mut f) => {
                        let _ = f.write_all(&input);
                    }
                    Err(e) => {
                        let _ = writeln!(stderr, "tee: '{file_str}': {e}");
                    }
                }
            }
        }

        Ok(results::ExecutionResult::new(0))
    }
}

/// `basename` — strip directory and suffix from filenames.
pub struct BasenameBuiltin;

impl SimpleCommand for BasenameBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: strip directory and suffix from filenames\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stdout = context.stdout();
        let mut stderr = context.stderr();

        let positional: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();

        if positional.is_empty() {
            let _ = writeln!(stderr, "basename: missing operand");
            return Ok(results::ExecutionResult::new(1));
        }

        let name = positional[0];
        let suffix = if positional.len() > 1 {
            positional[1]
        } else {
            ""
        };

        // Get the basename
        let result = std::path::Path::new(name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                // Handle cases like "/" -> "/"
                if name == "/" { "/".to_string() } else { String::new() }
            });

        // Strip suffix if provided and matching
        let result = if !suffix.is_empty() && result.ends_with(suffix) && result != suffix {
            result[..result.len() - suffix.len()].to_string()
        } else {
            result
        };

        let _ = writeln!(stdout, "{result}");
        Ok(results::ExecutionResult::new(0))
    }
}

/// `dirname` — strip last component from file name.
pub struct DirnameBuiltin;

impl SimpleCommand for DirnameBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: strip last component from file name\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let mut stdout = context.stdout();
        let mut stderr = context.stderr();

        let positional: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();

        if positional.is_empty() {
            let _ = writeln!(stderr, "dirname: missing operand");
            return Ok(results::ExecutionResult::new(1));
        }

        for name in &positional {
            let result = std::path::Path::new(name)
                .parent()
                .map(|p| {
                    let s = p.to_string_lossy().to_string();
                    if s.is_empty() { ".".to_string() } else { s }
                })
                .unwrap_or_else(|| ".".to_string());

            let _ = writeln!(stdout, "{result}");
        }

        Ok(results::ExecutionResult::new(0))
    }
}

/// `find` — search for files in a directory hierarchy.
/// Uses uutils/findutils with VFS integration on WASM.
pub struct FindBuiltin;

impl SimpleCommand for FindBuiltin {
    fn get_content(
        name: &str,
        _content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, error::Error> {
        Ok(format!("{name}: search for files in a directory hierarchy\n"))
    }

    fn execute<SE: extensions::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<results::ExecutionResult, error::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        let stdout = context.stdout();
        let _stderr = context.stderr();
        #[allow(unused_variables)]
        let shell = context.shell;

        // Convert args to &str for findutils
        let str_args: Vec<&str> = args.iter().map(String::as_str).collect();

        // Create a Dependencies implementation that writes to our stdout
        let deps = FindDeps::new(Box::new(stdout));

        #[cfg(target_family = "wasm")]
        let exit_code = {
            // Build the list_dir closure using the VFS
            let list_dir = |path: &str| -> Result<Vec<findutils::find::VfsEntry>, String> {
                let abs_path = shell.absolute_path(std::path::Path::new(path));
                match brush_core::wasm_list_dir(&abs_path) {
                    Some(entries) => Ok(entries
                        .into_iter()
                        .map(|(name, size, is_dir, modified_ms)| findutils::find::VfsEntry {
                            name,
                            is_dir,
                            size,
                            modified_ms,
                        })
                        .collect()),
                    None => Err(format!("cannot access '{path}': No such file or directory")),
                }
            };

            findutils::find::find_main_vfs(&str_args, &deps, &list_dir)
        };

        #[cfg(not(target_family = "wasm"))]
        let exit_code = findutils::find::find_main(&str_args, &deps);

        // Flush deps output to our stdout
        drop(deps);

        #[expect(clippy::cast_sign_loss)]
        Ok(results::ExecutionResult::new((exit_code & 0xFF) as u8))
    }
}

/// findutils Dependencies implementation that writes to a `Box<dyn Write>`.
struct FindDeps {
    output: std::cell::RefCell<Box<dyn IoWrite>>,
    now: std::time::SystemTime,
}

impl FindDeps {
    fn new(output: Box<dyn IoWrite>) -> Self {
        // On WASM, SystemTime::now() panics ("time not implemented on this
        // platform"), so we fall back to UNIX_EPOCH.  This means -mtime and
        // -newer won't produce meaningful results on WASM, but everything else
        // works.
        #[cfg(target_family = "wasm")]
        let now = std::time::SystemTime::UNIX_EPOCH;
        #[cfg(not(target_family = "wasm"))]
        let now = std::time::SystemTime::now();

        Self {
            output: std::cell::RefCell::new(output),
            now,
        }
    }
}

impl findutils::find::Dependencies for FindDeps {
    fn get_output(&self) -> &std::cell::RefCell<dyn IoWrite> {
        &self.output
    }

    fn now(&self) -> std::time::SystemTime {
        self.now
    }
}

/// Returns all uutils builtins as a `HashMap` suitable for
/// [`brush_core::ShellBuilder::builtins`].
pub fn uutils_builtins<SE: extensions::ShellExtensions>(
) -> HashMap<String, builtins::Registration<SE>> {
    let mut m = HashMap::new();

    m.insert("cat".into(), builtins::simple_builtin::<CatBuiltin, SE>());
    m.insert("echo".into(), builtins::simple_builtin::<EchoBuiltin, SE>());
    m.insert("sort".into(), builtins::simple_builtin::<SortBuiltin, SE>());
    m.insert("uniq".into(), builtins::simple_builtin::<UniqBuiltin, SE>());
    m.insert("wc".into(), builtins::simple_builtin::<WcBuiltin, SE>());
    m.insert("head".into(), builtins::simple_builtin::<HeadBuiltin, SE>());
    m.insert("tail".into(), builtins::simple_builtin::<TailBuiltin, SE>());
    m.insert("cut".into(), builtins::simple_builtin::<CutBuiltin, SE>());
    m.insert("tr".into(), builtins::simple_builtin::<TrBuiltin, SE>());
    m.insert("seq".into(), builtins::simple_builtin::<SeqBuiltin, SE>());
    m.insert("grep".into(), builtins::simple_builtin::<GrepBuiltin, SE>());
    m.insert("printf".into(), builtins::simple_builtin::<PrintfBuiltin, SE>());
    m.insert("ls".into(), builtins::simple_builtin::<LsBuiltin, SE>());
    m.insert("mkdir".into(), builtins::simple_builtin::<MkdirBuiltin, SE>());
    m.insert("rm".into(), builtins::simple_builtin::<RmBuiltin, SE>());
    m.insert("mv".into(), builtins::simple_builtin::<MvBuiltin, SE>());
    m.insert("touch".into(), builtins::simple_builtin::<TouchBuiltin, SE>());
    m.insert("tee".into(), builtins::simple_builtin::<TeeBuiltin, SE>());
    m.insert("basename".into(), builtins::simple_builtin::<BasenameBuiltin, SE>());
    m.insert("dirname".into(), builtins::simple_builtin::<DirnameBuiltin, SE>());
    m.insert("find".into(), builtins::simple_builtin::<FindBuiltin, SE>());

    m
}
