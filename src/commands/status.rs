use crate::{
    context::Ctx,
    db::SqliteDatabase,
    file_repository::{Change, FileRepository},
    paths::RPath,
};
use std::io::Write as _;

#[derive(clap::Args)]
pub struct StatusArgs;

pub fn local_status<R: FileRepository>(ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, local, .. } = ctx;
    let status_of_dir_future = status_of_dir(&ctx.env.db, local, &ctx.env.wd);
    env.shell.print_transient("Reading directories...")?;
    let res = env.handle.block_on(status_of_dir_future)?;
    writeln!(env.shell.err(), "{res:?}")?;

    Ok(())
}

pub fn remote_status<R: FileRepository>(ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, remote, .. } = ctx;
    let status_of_dir_future = status_of_dir(&ctx.env.db, remote, &ctx.env.wd);
    env.shell.print_transient("Reading directories...")?;
    let res = env.handle.block_on(status_of_dir_future)?;
    writeln!(env.shell.err(), "{res:?}")?;

    Ok(())
}

pub(super) async fn status_of_dir<'a>(
    db: &SqliteDatabase,
    repo: &'a impl FileRepository,
    dir: &RPath,
) -> anyhow::Result<Vec<Change>> {
    // Get a list of files...
    let all_files = db.get_all_tracked(dir)?;
    let res = repo.determine_changes(dir, all_files).await?;
    Ok(res)
}
