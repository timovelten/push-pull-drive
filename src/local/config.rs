use super::{Config, LocalBackend};
use crate::context::ExecutionEnvironment;
use std::{path::PathBuf, sync::Arc};

#[derive(clap::Args, Clone)]
pub struct LocalInitArgs {
    /// Path of the "remote" folder we push to and pull from.
    path: String,
}

pub fn init(ctx: &ExecutionEnvironment, args: LocalInitArgs) -> anyhow::Result<()> {
    let root_path: PathBuf = args.path.into();
    let config = Config {
        root_path,
        ignore_prefixes: vec![],
    };

    ctx.config.edit_backend_config(|_| Ok(config))?;

    Ok(())
}

// Note: This is code meant to enable us to use the local filesystem as a "remote"
// folder. In this case, the hasher is fixed (a particular implementation of FileRepository can have
// only one hasher associated with it) as SHA1.

pub fn load(ctx: &ExecutionEnvironment) -> anyhow::Result<Option<LocalBackend>> {
    let Some(config) = ctx.config.backend_config()? else {
        return Ok(None);
    };

    Ok(Some(LocalBackend {
        config: Arc::new(config),
    }))
}
