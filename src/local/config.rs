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

    ctx.db.put_value("local", &config)?;

    Ok(())
}

// Note: This is code meant to enable us to use the local filesystem as a "remote"
// folder. In this case, the hasher is fixed (a particular implementation of FileRepository can have
// only one hasher associated with it) as SHA1.

pub fn load(ctx: &ExecutionEnvironment) -> anyhow::Result<Option<LocalBackend>> {
    let config= match ctx.db.get_value::<Config>("local")? {
        Some(config) => config,
        None => return Ok(None)
    };

    Ok(Some(LocalBackend {
        config: Arc::new(config),
    }))
}
