use crate::core::error::{CoreError, CoreResult};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub program: String,
    pub args: Vec<String>,
    pub status: Option<i32>,
    pub success: bool,
    /// stdout and stderr combined (in capture order) for progress commands.
    pub log: String,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn command_line(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(' ') {
                out.push('\'');
                out.push_str(arg);
                out.push('\'');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

pub fn run(program: &Path, args: &[String]) -> CoreResult<CommandOutput> {
    let mut command = Command::new(program);
    command.args(args).env("LC_ALL", "C").stdin(Stdio::null());

    let output = command.output().map_err(|e| {
        CoreError::new(format!(
            "Não foi possível executar '{}': {e}",
            program.display()
        ))
    })?;

    Ok(CommandOutput {
        program: program.display().to_string(),
        args: args.to_vec(),
        status: output.status.code(),
        success: output.status.success(),
        log: String::new(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Runs a process while polling a value (usually the size of an output file).
/// The child's stdout and stderr are written to a temporary log to avoid pipe
/// deadlocks and are returned in `log`.
pub fn run_polling<F, P>(
    program: &Path,
    args: &[String],
    cancel: &CancelToken,
    mut poll: P,
    mut on_progress: F,
) -> CoreResult<CommandOutput>
where
    F: FnMut(u64),
    P: FnMut() -> u64,
{
    let log = NamedTempFile::new()
        .map_err(|e| CoreError::new(format!("Não foi possível criar log temporário: {e}")))?;
    let out = log
        .reopen()
        .map_err(|e| CoreError::new(format!("Não foi possível abrir log temporário: {e}")))?;
    let err = out
        .try_clone()
        .map_err(|e| CoreError::new(format!("Não foi possível compartilhar log: {e}")))?;

    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    let mut child = command.spawn().map_err(|e| {
        CoreError::new(format!(
            "Não foi possível executar '{}': {e}",
            program.display()
        ))
    })?;

    let status = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreError::new("Operação cancelada pelo usuário."));
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoreError::new(format!(
                    "Falha ao aguardar '{}': {e}",
                    program.display()
                )));
            }
        }

        on_progress(poll());
        sleep(Duration::from_millis(200));
    };

    let log_text = std::fs::read_to_string(log.path()).unwrap_or_default();

    Ok(CommandOutput {
        program: program.display().to_string(),
        args: args.to_vec(),
        status: status.code(),
        success: status.success(),
        log: log_text,
        stdout: String::new(),
        stderr: String::new(),
    })
}

fn env_override(name: &str) -> Option<PathBuf> {
    let key = format!("ISOFORGE_{}", name.to_ascii_uppercase().replace('-', "_"));
    let value = std::env::var_os(key)?;
    let path = PathBuf::from(value);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// Finds a tool using, in order:
/// 1. an `ISOFORGE_<TOOL>` environment override,
/// 2. `PATH`,
/// 3. known local fallback locations used by the project documentation.
pub fn locate_tool(name: &str) -> Option<PathBuf> {
    if let Some(path) = env_override(name) {
        return Some(path);
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let mut fallbacks = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        fallbacks.push(home.join(".local/opt/isoforge-tools/usr/bin").join(name));
        fallbacks.push(home.join(".local/opt/isoforge").join(name));
    }
    fallbacks.push(PathBuf::from("/usr/local/bin").join(name));
    fallbacks.push(PathBuf::from("/usr/bin").join(name));

    fallbacks.into_iter().find(|path| path.is_file())
}

pub fn available_space(path: &Path) -> CoreResult<u64> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| CoreError::new("Caminho inválido para verificação de espaço."))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(CoreError::new(format!(
            "Não foi possível verificar o espaço livre em '{}': {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

pub fn workspace_root() -> CoreResult<PathBuf> {
    let root = if let Some(path) = std::env::var_os("ISOFORGE_WORK_DIR") {
        PathBuf::from(path)
    } else {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        base.join("isoforge")
    };

    std::fs::create_dir_all(&root).map_err(|e| {
        CoreError::new(format!(
            "Não foi possível criar o diretório de trabalho '{}': {e}",
            root.display()
        ))
    })?;
    Ok(root)
}

pub fn create_workspace() -> CoreResult<tempfile::TempDir> {
    let root = workspace_root()?;
    tempfile::Builder::new()
        .prefix("isoforge-")
        .tempdir_in(&root)
        .map_err(|e| {
            CoreError::new(format!(
                "Não foi possível criar workspace em '{}': {e}",
                root.display()
            ))
        })
}
