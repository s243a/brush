//! Filesystem interaction in the shell.

#[cfg(not(target_family = "wasm"))]
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use normalize_path::NormalizePath as _;

use crate::{
    ExecutionParameters, ShellFd,
    env::{EnvironmentLookup, EnvironmentScope},
    error, openfiles, pathsearch,
    sys::{fs::PathExt as _, users},
    variables,
};

// ── WASM VFS hook ───────────────────────────────────────────────
// On WASM, file I/O delegates to a JS-backed VFS via a thread-local
// callback set by brush-wasm during initialization.

#[cfg(target_family = "wasm")]
type VfsOpenFn = Box<dyn Fn(&Path, bool, bool, bool, bool) -> Result<openfiles::OpenFile, std::io::Error>>;

#[cfg(target_family = "wasm")]
type VfsExistsFn = Box<dyn Fn(&Path) -> bool>;

/// VFS directory entry: (name, size, is_dir, modified_ms)
#[cfg(target_family = "wasm")]
type VfsListDirFn = Box<dyn Fn(&Path) -> Option<Vec<(String, u64, bool, u64)>>>;

#[cfg(target_family = "wasm")]
type VfsRemoveFn = Box<dyn Fn(&Path) -> bool>;

/// VFS stat result: (size, is_dir, modified_ms)
#[cfg(target_family = "wasm")]
type VfsStatFn = Box<dyn Fn(&Path) -> Option<(u64, bool, u64)>>;

#[cfg(target_family = "wasm")]
type VfsMkdirFn = Box<dyn Fn(&Path)>;

#[cfg(target_family = "wasm")]
thread_local! {
    /// VFS open callback: (path, read, write, create, append) -> Result<OpenFile>
    static VFS_OPEN: std::cell::RefCell<Option<VfsOpenFn>> = const { std::cell::RefCell::new(None) };
    /// VFS exists callback: (path) -> bool
    static VFS_EXISTS: std::cell::RefCell<Option<VfsExistsFn>> = const { std::cell::RefCell::new(None) };
    /// VFS list_dir callback: (path) -> Option<Vec<(name, size, is_dir, modified_ms)>>
    static VFS_LIST_DIR: std::cell::RefCell<Option<VfsListDirFn>> = const { std::cell::RefCell::new(None) };
    /// VFS remove callback: (path) -> bool
    static VFS_REMOVE: std::cell::RefCell<Option<VfsRemoveFn>> = const { std::cell::RefCell::new(None) };
    /// VFS stat callback: (path) -> Option<(size, is_dir, modified_ms)>
    static VFS_STAT: std::cell::RefCell<Option<VfsStatFn>> = const { std::cell::RefCell::new(None) };
    /// VFS mkdir callback: (path)
    static VFS_MKDIR: std::cell::RefCell<Option<VfsMkdirFn>> = const { std::cell::RefCell::new(None) };
}

/// Register VFS callbacks. Called by brush-wasm during init.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn set_wasm_vfs(
    open_fn: impl Fn(&Path, bool, bool, bool, bool) -> Result<openfiles::OpenFile, std::io::Error> + 'static,
    exists_fn: impl Fn(&Path) -> bool + 'static,
) {
    VFS_OPEN.with(|cell| *cell.borrow_mut() = Some(Box::new(open_fn)));
    VFS_EXISTS.with(|cell| *cell.borrow_mut() = Some(Box::new(exists_fn)));
}

/// Register extended VFS callbacks for directory listing, remove, stat, mkdir.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn set_wasm_vfs_extended(
    list_dir_fn: impl Fn(&Path) -> Option<Vec<(String, u64, bool, u64)>> + 'static,
    remove_fn: impl Fn(&Path) -> bool + 'static,
    stat_fn: impl Fn(&Path) -> Option<(u64, bool, u64)> + 'static,
    mkdir_fn: impl Fn(&Path) + 'static,
) {
    VFS_LIST_DIR.with(|cell| *cell.borrow_mut() = Some(Box::new(list_dir_fn)));
    VFS_REMOVE.with(|cell| *cell.borrow_mut() = Some(Box::new(remove_fn)));
    VFS_STAT.with(|cell| *cell.borrow_mut() = Some(Box::new(stat_fn)));
    VFS_MKDIR.with(|cell| *cell.borrow_mut() = Some(Box::new(mkdir_fn)));
}

/// Open a file for reading via the VFS. Returns `Box<dyn Read>` for use
/// by uutils builtins that need to open files by path on WASM.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_open_file_for_read(path: &Path) -> std::io::Result<Box<dyn std::io::Read>> {
    VFS_OPEN.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref open_fn) = *borrow {
            let open_file = open_fn(path, true, false, false, false)?;
            Ok(Box::new(open_file) as Box<dyn std::io::Read>)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no VFS registered",
            ))
        }
    })
}

/// Check if a file exists via the VFS. For use by uutils builtins on WASM.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_file_exists(path: &Path) -> bool {
    VFS_EXISTS.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref exists_fn) = *borrow {
            exists_fn(path)
        } else {
            false
        }
    })
}

/// List directory contents via the VFS. Returns Vec of (name, size, is_dir, modified_ms).
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_list_dir(path: &Path) -> Option<Vec<(String, u64, bool, u64)>> {
    VFS_LIST_DIR.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref list_fn) = *borrow {
            list_fn(path)
        } else {
            None
        }
    })
}

/// Remove a file or empty directory via the VFS.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_remove(path: &Path) -> bool {
    VFS_REMOVE.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref rm_fn) = *borrow {
            rm_fn(path)
        } else {
            false
        }
    })
}

/// Get file/directory stat via the VFS. Returns (size, is_dir, modified_ms).
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_stat(path: &Path) -> Option<(u64, bool, u64)> {
    VFS_STAT.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref stat_fn) = *borrow {
            stat_fn(path)
        } else {
            None
        }
    })
}

/// Create a directory via the VFS.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_mkdir(path: &Path) {
    VFS_MKDIR.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref mkdir_fn) = *borrow {
            mkdir_fn(path);
        }
    })
}

/// Open a file via the VFS with explicit mode flags.
/// Returns an OpenFile that can be written to.
#[cfg(target_family = "wasm")]
#[allow(dead_code)]
pub fn wasm_open_file(
    path: &Path,
    read: bool,
    write: bool,
    create: bool,
    append: bool,
) -> std::io::Result<openfiles::OpenFile> {
    VFS_OPEN.with(|cell| {
        let borrow = cell.borrow();
        if let Some(ref open_fn) = *borrow {
            open_fn(path, read, write, create, append)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no VFS registered",
            ))
        }
    })
}

/// Explicit file open mode flags for WASM VFS delegation.
#[derive(Debug, Clone, Default)]
pub struct FileOpenMode {
    /// Open for reading.
    pub read: bool,
    /// Open for writing.
    pub write: bool,
    /// Create the file if it doesn't exist.
    pub create: bool,
    /// Append to the file instead of truncating.
    pub append: bool,
}

/// Split a PATH-like string into individual directories.
/// On native platforms, delegates to `std::env::split_paths`.
/// On WASM, `std::env::split_paths` panics, so we fall back to
/// splitting on `:` (the POSIX path separator).
fn split_paths(s: &str) -> impl Iterator<Item = PathBuf> + '_ {
    #[cfg(not(target_family = "wasm"))]
    {
        std::env::split_paths(OsStr::new(s))
    }
    #[cfg(target_family = "wasm")]
    {
        s.split(':').map(PathBuf::from).collect::<Vec<_>>().into_iter()
    }
}

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Sets the shell's current working directory to the given path.
    ///
    /// # Arguments
    ///
    /// * `target_dir` - The path to set as the working directory.
    pub fn set_working_dir(&mut self, target_dir: impl AsRef<Path>) -> Result<(), error::Error> {
        let abs_path = self.absolute_path(target_dir.as_ref());

        // On WASM, skip metadata check if VFS is available (it manages directories)
        #[cfg(target_family = "wasm")]
        {
            let vfs_ok = VFS_EXISTS.with(|cell| {
                let borrow = cell.borrow();
                if let Some(ref exists_fn) = *borrow {
                    // If VFS knows the path exists (or it's root), allow the cd
                    exists_fn(&abs_path) || abs_path.to_str() == Some("/")
                } else {
                    false
                }
            });
            if !vfs_ok {
                // Fall through to std::fs check below (will likely fail on WASM)
            } else {
                // Skip std::fs::metadata check on WASM, go straight to setting the dir
                let cleaned_path = abs_path.normalize();
                let pwd = cleaned_path.to_string_lossy().to_string();
                self.env.update_or_add("PWD", variables::ShellValueLiteral::Scalar(pwd), |_| Ok(()), EnvironmentLookup::Anywhere, EnvironmentScope::Global)?;
                let oldpwd = std::mem::replace(self.working_dir_mut(), cleaned_path);
                self.env.update_or_add("OLDPWD", variables::ShellValueLiteral::Scalar(oldpwd.to_string_lossy().to_string()), |_| Ok(()), EnvironmentLookup::Anywhere, EnvironmentScope::Global)?;
                return Ok(());
            }
        }

        match std::fs::metadata(&abs_path) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(error::ErrorKind::NotADirectory(abs_path).into());
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }

        // Normalize the path (but don't canonicalize it).
        let cleaned_path = abs_path.normalize();

        let pwd = cleaned_path.to_string_lossy().to_string();

        self.env.update_or_add(
            "PWD",
            variables::ShellValueLiteral::Scalar(pwd),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
        let oldpwd = std::mem::replace(self.working_dir_mut(), cleaned_path);

        self.env.update_or_add(
            "OLDPWD",
            variables::ShellValueLiteral::Scalar(oldpwd.to_string_lossy().to_string()),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;

        Ok(())
    }

    /// Tilde-shortens the given string, replacing the user's home directory with a tilde.
    ///
    /// # Arguments
    ///
    /// * `s` - The string to shorten.
    pub fn tilde_shorten(&self, s: String) -> String {
        if let Some(home_dir) = self.home_dir()
            && let Some(stripped) = s.strip_prefix(home_dir.to_string_lossy().as_ref())
        {
            return format!("~{stripped}");
        }
        s
    }

    /// Returns the shell's current home directory, if available.
    pub(crate) fn home_dir(&self) -> Option<PathBuf> {
        if let Some(home) = self.env.get_str("HOME", self) {
            Some(PathBuf::from(home.to_string()))
        } else {
            // HOME isn't set, so let's sort it out ourselves.
            users::get_current_user_home_dir()
        }
    }

    /// Finds executables in the shell's current default PATH, matching the given glob pattern.
    ///
    /// # Arguments
    ///
    /// * `required_glob_pattern` - The glob pattern to match against.
    pub fn find_executables_in_path<'a>(
        &'a self,
        filename: &'a str,
    ) -> impl Iterator<Item = PathBuf> + 'a {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = split_paths(path_var.as_ref());

        pathsearch::search_for_executable(paths, filename)
    }

    /// Finds executables in the shell's current default PATH, with filenames matching the
    /// given prefix.
    ///
    /// # Arguments
    ///
    /// * `filename_prefix` - The prefix to match against executable filenames.
    pub fn find_executables_in_path_with_prefix(
        &self,
        filename_prefix: &str,
        case_insensitive: bool,
    ) -> impl Iterator<Item = PathBuf> {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = split_paths(path_var.as_ref());

        pathsearch::search_for_executable_with_prefix(paths, filename_prefix, case_insensitive)
    }

    /// Determines whether the given filename is the name of an executable in one of the
    /// directories in the shell's current PATH. If found, returns the path.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path<S: AsRef<str>>(
        &self,
        candidate_name: S,
    ) -> Option<PathBuf> {
        let path = self.env_str("PATH").unwrap_or_default();
        for one_dir in split_paths(path.as_ref()) {
            let candidate_path = one_dir.join(candidate_name.as_ref());
            if candidate_path.executable() {
                return Some(candidate_path);
            }
        }
        None
    }

    /// Uses the shell's hash-based path cache to check whether the given filename is the name
    /// of an executable in one of the directories in the shell's current PATH. If found,
    /// ensures the path is in the cache and returns it.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path_using_cache<S: AsRef<str>>(
        &mut self,
        candidate_name: S,
    ) -> Option<PathBuf>
    where
        String: From<S>,
    {
        if let Some(cached_path) = self.program_location_cache.get(&candidate_name) {
            Some(cached_path)
        } else if let Some(found_path) = self.find_first_executable_in_path(&candidate_name) {
            self.program_location_cache
                .set(candidate_name, found_path.clone());
            Some(found_path)
        } else {
            None
        }
    }

    /// Gets the absolute form of the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to get the absolute form of.
    pub fn absolute_path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.is_absolute() {
            path.to_owned()
        } else {
            self.working_dir().join(path)
        }
    }

    /// Opens the given file, using the context of this shell and the provided execution parameters.
    ///
    /// # Arguments
    ///
    /// * `options` - The options to use opening the file.
    /// * `path` - The path to the file to open; may be relative to the shell's working directory.
    /// * `params` - Execution parameters.
    pub(crate) fn open_file(
        &self,
        options: &std::fs::OpenOptions,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
    ) -> Result<openfiles::OpenFile, std::io::Error> {
        // Default to read mode — all callers of open_file() use it for reading.
        // On WASM, the VFS needs explicit mode flags since OpenOptions fields are private.
        self.open_file_with_mode(
            options,
            path,
            params,
            Some(FileOpenMode { read: true, ..Default::default() }),
        )
    }

    /// Open a file with explicit mode flags. On WASM, the mode flags are used
    /// to delegate to the JS-backed VFS (since std::fs::OpenOptions fields are private).
    pub(crate) fn open_file_with_mode(
        &self,
        options: &std::fs::OpenOptions,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
        #[cfg_attr(not(target_family = "wasm"), allow(unused_variables))]
        mode: Option<FileOpenMode>,
    ) -> Result<openfiles::OpenFile, std::io::Error> {
        let path_to_open = self.absolute_path(path.as_ref());

        // See if this is a reference to a file descriptor, in which case the actual
        // /dev/fd* file path for this process may not match with what's in the execution
        // parameters.
        if let Some(parent) = path_to_open.parent()
            && parent == Path::new("/dev/fd")
            && let Some(filename) = path_to_open.file_name()
            && let Ok(fd_num) = filename.to_string_lossy().to_string().parse::<ShellFd>()
            && let Some(open_file) = params.try_fd(self, fd_num)
        {
            return open_file.try_clone();
        }

        // On WASM, delegate to the JS-backed VFS if registered.
        #[cfg(target_family = "wasm")]
        {
            let m = mode.unwrap_or_default();
            let result = VFS_OPEN.with(|cell| {
                let borrow = cell.borrow();
                if let Some(ref open_fn) = *borrow {
                    Some(open_fn(&path_to_open, m.read, m.write, m.create, m.append))
                } else {
                    None
                }
            });

            if let Some(r) = result {
                return r;
            }
        }

        Ok(options.open(path_to_open)?.into())
    }

    /// Replaces the shell's currently configured open files with the given set.
    /// Typically only used by exec-like builtins.
    ///
    /// # Arguments
    ///
    /// * `open_files` - The new set of open files to use.
    pub fn replace_open_files(
        &mut self,
        open_fds: impl Iterator<Item = (ShellFd, openfiles::OpenFile)>,
    ) {
        self.open_files = openfiles::OpenFiles::from(open_fds);
    }

    pub(crate) const fn persistent_open_files(&self) -> &openfiles::OpenFiles {
        &self.open_files
    }
}
