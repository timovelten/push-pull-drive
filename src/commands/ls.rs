use crate::{
    context::{Context, Ctx}, file_repository::FileRepository, paths::{RPath, RPathBuf}, DirectoryInfo, FileInfo, PathInfo
};
use std::io::Write;

#[derive(clap::Args)]
pub struct LsArgs {
    path: Option<RPathBuf>,

    #[arg(long)]
    depth: Option<u32>,

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

    env.shell.print_transient("Reading directory...")?;

    let path = env.wd().join(path.as_deref().unwrap_or(RPath::ROOT));
    let mut contents = env
        .handle
        .block_on(remote.read_dir(&path, depth.unwrap_or(1)))?;

    // Sort directories first
    fn key(f: &PathInfo) -> (u8, &RPath) {
        match f {
            PathInfo::Dir(DirectoryInfo {
                rpath: relative_name,
            }) => (0, relative_name),
            PathInfo::File(FileInfo { rpath, .. }) => (1, rpath),
        }
    }
    contents.sort_by(|f, g| key(f).cmp(&key(g)));

    for file in contents {
        match file {
            PathInfo::Dir(DirectoryInfo {
                rpath: relative_name,
            }) => {
                writeln!(env.shell.out(), "{}/", relative_name)?;
            }
            PathInfo::File(FileInfo {
                rpath, size, hash, ..
            }) => {
                if details {
                    let short_hash = hex::encode(&hash[..hash.len().min(4)]);
                    writeln!(env.shell.out(), "{rpath} [{size} b, Hash: {short_hash}]")?;
                } else {
                    writeln!(env.shell.out(), "{rpath}")?;
                }
            }
        }
    }

    Ok(())
}
