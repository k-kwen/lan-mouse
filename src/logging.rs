use env_logger::{Env, Target};
use lan_mouse::config::LogArgs;
use std::{fs::OpenOptions, io, path::Path};

const LOG_LEVEL_ENV: &str = "LAN_MOUSE_LOG_LEVEL";

pub(crate) fn init() -> Result<(), io::Error> {
    let options = LogArgs::options_from_env_and_args();
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

fn ensure_parent(path: &Path) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
