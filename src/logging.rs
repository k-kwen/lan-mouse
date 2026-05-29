use env_logger::{Env, Target};
use std::{
    env,
    ffi::OsString,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
};

const LOG_LEVEL_ENV: &str = "LAN_MOUSE_LOG_LEVEL";
const LOG_FILE_ENV: &str = "LAN_MOUSE_LOG_FILE";

pub(crate) fn init() -> Result<(), io::Error> {
    let options = LogOptions::from_env_and_args();
    let default_level = options.level.unwrap_or_else(|| "info".to_owned());
    let env = Env::default().filter_or(LOG_LEVEL_ENV, default_level);
    let mut builder = env_logger::Builder::from_env(env);

    if let Some(path) = options.file {
        ensure_parent(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        builder.target(Target::Pipe(Box::new(file)));
    }

    builder.init();
    Ok(())
}

#[derive(Debug, Default)]
struct LogOptions {
    file: Option<PathBuf>,
    level: Option<String>,
}

impl LogOptions {
    fn from_env_and_args() -> Self {
        let file = arg_value("--log-file")
            .map(PathBuf::from)
            .or_else(|| env::var_os(LOG_FILE_ENV).map(PathBuf::from));
        let level = arg_value("--log-level")
            .and_then(|v| v.into_string().ok())
            .or_else(|| env::var(LOG_LEVEL_ENV).ok());
        Self { file, level }
    }
}

fn arg_value(name: &str) -> Option<OsString> {
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(value) = split_inline_arg(&arg, name) {
            return Some(value);
        }
    }
    None
}

fn split_inline_arg(arg: &OsString, name: &str) -> Option<OsString> {
    let arg = arg.to_str()?;
    let (key, value) = arg.split_once('=')?;
    (key == name).then(|| OsString::from(value))
}

fn ensure_parent(path: &Path) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_inline_arg;
    use std::ffi::OsString;

    #[test]
    fn split_inline_arg_accepts_exact_match() {
        assert_eq!(
            split_inline_arg(&OsString::from("--log-file=daemon.log"), "--log-file"),
            Some(OsString::from("daemon.log"))
        );
    }

    #[test]
    fn split_inline_arg_rejects_prefix_match() {
        assert_eq!(
            split_inline_arg(&OsString::from("--log-file-extra=daemon.log"), "--log-file"),
            None
        );
    }
}
