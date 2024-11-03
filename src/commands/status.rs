use crate::{
    context::Ctx, db::SqliteDatabase, file_repository::FileRepository, filetree::Patch, paths::RPath
};

#[derive(clap::Args)]
pub struct StatusArgs;

pub fn local_status<R: FileRepository>(ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, local, .. } = ctx;
    let status_of_dir_future = status_of_dir(
        &ctx.env.db,
        local,
        &ctx.env.wd
    );
    env.shell.print_transient("Reading directories...")?;
    let res = env.handle.block_on(status_of_dir_future)?;
    println!("{res:#?}");

    Ok(())
}

pub fn remote_status<R: FileRepository>(ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, remote, .. } = ctx;
    let status_of_dir_future = status_of_dir(
        &ctx.env.db,
        remote,
        &ctx.env.wd
    );
    env.shell.print_transient("Reading directories...")?;
    let res = env.handle.block_on(status_of_dir_future)?;
    println!("{res:#?}");

    Ok(())
}

pub(super) async fn status_of_dir<'a>(
    db: &SqliteDatabase,
    repo: &'a impl FileRepository,
    dir: &RPath
) -> anyhow::Result<Vec<Patch<'a>>> {
    // Get a list of files...
    let all_files = db.get_all_tracked(dir)?;
    let res = repo.determine_changes(dir, all_files).await?;

    Ok(res)
}
