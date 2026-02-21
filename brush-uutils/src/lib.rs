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

                // Install VFS file hooks so builtins can open files by path on WASM.
                #[cfg(target_family = "wasm")]
                uucore::wasm_io::set_file_hooks(
                    Box::new(|path| brush_core::wasm_open_file_for_read(path)),
                    Box::new(|path| brush_core::wasm_file_exists(path)),
                );

                let exit_code = uucore::wasm_io::with_wasm_io(
                    Box::new(context.stdin()),
                    Box::new(context.stdout()),
                    Box::new(context.stderr()),
                    || {
                        use $crate_path as uu;
                        uu::uumain(os_args.into_iter())
                    },
                );

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

    m
}
