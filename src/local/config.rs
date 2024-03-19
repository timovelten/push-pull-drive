use anyhow::anyhow;

use crate::{context::CoreContext, KEY_BACKEND};

use super::LocalBackend;

const LOCAL_BACKEND: &str = "local";
const KEY_LOCAL_PATH: &str = "local_path";

#[derive(clap::Args, Clone)]
pub struct LocalInitArgs {
    /// Path of the "remote" folder we push to and pull from.
    path: String,
}

pub fn init(ctx: &CoreContext, args: LocalInitArgs) -> anyhow::Result<()> {
    ctx.key_value_store.put(KEY_BACKEND, LOCAL_BACKEND)?;
    ctx.key_value_store.put(KEY_LOCAL_PATH, &args.path)?;

    Ok(())
}

// Note: This is code meant to enable us to use the local filesystem as a "remote"
// folder. In this case, the hasher is fixed (a particular implementation of FileRepository can have
// only one hasher associated with it) as SHA1.

pub fn load(ctx: &CoreContext) -> anyhow::Result<Option<LocalBackend>> {
    if ctx.key_value_store.get(KEY_BACKEND)?.as_deref() == Some(LOCAL_BACKEND) {
        Ok(Some(load_internal(ctx)?))
    } else {
        Ok(None)
    }
}

fn load_internal(ctx: &CoreContext) -> anyhow::Result<LocalBackend> {
    log::debug!("Loading local backend");

    let local_path = ctx
        .key_value_store
        .get(KEY_LOCAL_PATH)?
        .ok_or_else(|| anyhow!("Expected to find {KEY_LOCAL_PATH}"))?;
    let config = super::Config {
        root_path: local_path.into(),
        ..Default::default()
    };
    Ok(LocalBackend::from_config(config))
}
