use crate::{
    context::{Context, Ctx},
    db::Side, paths::RPathBuf,
};

#[derive(clap::Args, Clone)]
pub struct ForgetArgs {
    /// The relative path that should be deleted from the index ("forgotten")
    path: RPathBuf,
}

pub fn forget(ctx: &impl Context, ForgetArgs { path }: ForgetArgs) -> anyhow::Result<()> {
    let Ctx { env, .. } = ctx.get();

    let rpath = if path.as_str() == "." {
        env.wd.to_owned()
    } else {
        env.wd.join(&path)
    };

    let files = env.tracked_files.get_all(&rpath, Side::Local)?;
    for file in files {
        env.tracked_files.delete(&file.rpath)?;
    }

    Ok(())
}
