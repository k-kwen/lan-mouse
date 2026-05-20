use env_logger::{Env, Target};
use std::{
    env, fs,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
};

const LOG_LEVEL_ENV: &str = "LAN_MOUSE_LOG_LEVEL";
const LOG_FILE_ENV: &str = "LAN_MOUSE_LOG_FILE";

pub(crate) fn init() -> Result<(), io::Error> {
    let level = env::var(LOG_LEVEL_ENV).unwrap_or_else(|_| "info".to_owned());
    let env = Env::default().filter_or(LOG_LEVEL_ENV, level);
    let mut builder = env_logger::Builder::from_env(env);

    if let Some(path) = env::var_os(LOG_FILE_ENV).map(PathBuf::from) {
        ensure_parent(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        builder.target(Target::Pipe(Box::new(file)));
    }

    builder.init();
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}
