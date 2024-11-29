use crate::{
    context::{Context, Ctx}, file_repository::FileRepository, human_readable_bytes, paths::{RPath, RPathBuf}, DirectoryInfo, FileInfo, PathInfo
};
use std::io::Write;

#[derive(clap::Args)]
pub struct LsArgs {
    path: Option<String>,

    #[arg(long)]
    depth: Option<usize>,

    /// Whether to print details such as size and hash of the files
    #[arg(long)]
    details: bool,
}

pub fn remote_ls(
    ctx: &impl Context,
    LsArgs {
        path,
        depth,
        details,
    }: LsArgs,
) -> anyhow::Result<()> {
    let Ctx { env, remote, .. } = ctx.get();
    let depth = depth.unwrap_or(1);

    env.shell.print_transient("Reading directory...")?;

    let path = path.map(|path| RPathBuf::from_sep(&path, '/'))
        .unwrap_or(RPathBuf::empty());
    let path = env.wd.join(&path);
    let mut contents = env
        .handle
        .block_on(remote.list_files(&path, depth))?;

    // Sort directories first
    fn key(f: &PathInfo) -> (u8, &RPath) {
        match f {
            PathInfo::Dir(DirectoryInfo {
                path: relative_name,
            }) => (0, relative_name),
            PathInfo::File(FileInfo { path: rpath, .. }) => (1, rpath),
        }
    }
    contents.sort_by(|f, g| key(f).cmp(&key(g)));

    for file in contents {
        match file {
            PathInfo::Dir(DirectoryInfo {
                path: relative_name,
            }) => {
                writeln!(env.shell.out(), "{}/", relative_name)?;
            }
            PathInfo::File(FileInfo {
                path: rpath, size, ..
            }) => {
                if details {
                    let (num, unit) = human_readable_bytes(size);
                    writeln!(env.shell.out(), "{rpath} [{num} {unit}]")?;
                } else {
                    writeln!(env.shell.out(), "{rpath}")?;
                }
            }
        }
    }

    Ok(())
}
