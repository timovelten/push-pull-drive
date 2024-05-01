use crate::{
    append_to_relative_name,
    context::{Ctx, FullContext},
    db::Side,
};

#[derive(clap::Args, Clone)]
pub struct ForgetArgs {
    /// The relative path that should be deleted from the index ("forgotten")
    path: String,
}

pub fn forget(ctx: &impl Ctx, ForgetArgs { path }: ForgetArgs) -> anyhow::Result<()> {
    let FullContext { ctx, .. } = ctx.get();
    let path = path.trim();

    let cpath = if path == "." {
        ctx.wd.to_owned()
    } else {
        append_to_relative_name(&ctx.wd, path)
    };

    let files = ctx.tracked_files.get_all(&cpath, Side::Local)?;
    for file in files {
        ctx.tracked_files.delete(&file.cpath)?;
    }

    Ok(())
}
