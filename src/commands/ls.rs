use crate::{
    FileInfo, PathInfo,
    context::{Context, Ctx},
    file_repository::FileRepository,
    human_readable_bytes,
    paths::{RPath, RPathBuf},
};
use std::io::Write;

#[derive(clap::Args)]
pub struct LsArgs {
    path: Option<String>,

    #[arg(long)]
    depth: Option<usize>,

    /// Whether to print details such as size and modified times of the files
    #[arg(long)]
    details: bool,
}

pub async fn remote_ls(
    ctx: &impl Context,
    LsArgs {
        path,
        depth,
        details,
    }: LsArgs,
) -> anyhow::Result<()> {
    let Ctx { env, remote, .. } = ctx.get();
    let depth = depth.unwrap_or(1);

    env.print_transient("Reading directory...")?;

    let path = path
        .map(|path| RPathBuf::deserialize_sloppy(&path))
        .unwrap_or(RPathBuf::empty());
    let path = env.wd.join(&path);
    let mut contents = remote.list_files(&path, depth).await?;

    // Sort directories first
    fn key(f: &PathInfo) -> (u8, &RPath) {
        match f {
            PathInfo::Dir(path) => (0, path),
            PathInfo::File(FileInfo { path: rpath, .. }) => (1, rpath),
        }
    }
    contents.sort_by(|f, g| key(f).cmp(&key(g)));

    for file in contents {
        match file {
            PathInfo::Dir(path) => {
                writeln!(env.out(), "{}/", path)?;
            }
            PathInfo::File(FileInfo {
                path: rpath, size, ..
            }) => {
                if details {
                    let (num, unit) = human_readable_bytes(size);
                    writeln!(env.out(), "{rpath} [{num} {unit}]")?;
                } else {
                    writeln!(env.out(), "{rpath}")?;
                }
            }
        }
    }

    Ok(())
}
