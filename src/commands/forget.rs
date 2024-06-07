use crate::{
    append_to_relative_name,
    context::{Context, Ctx},
    db::Side,
};

#[derive(clap::Args, Clone)]
pub struct ForgetArgs {
    /// The relative path that should be deleted from the index ("forgotten")
    path: String,
}

pub fn forget(ctx: &impl Context, ForgetArgs { path }: ForgetArgs) -> anyhow::Result<()> {
    let Ctx { env, .. } = ctx.get();
    let path = path.trim();

    let cpath = if path == "." {
        env.wd.to_owned()
    } else {
        append_to_relative_name(&env.wd, path)
    };

    let files = env.tracked_files.get_all(&cpath, Side::Local)?;
    for file in files {
        env.tracked_files.delete(&file.cpath)?;
    }

    Ok(())
}
