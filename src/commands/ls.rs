use crate::{
    append_to_relative_name,
    context::{Context, Ctx},
    file_repository::FileRepository,
    DirectoryInfo, FileInfo, PathInfo,
};
use std::io::Write;

#[derive(clap::Args)]
pub struct LsArgs {
    path: Option<String>,

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

    let path = append_to_relative_name(env.wd(), path.as_deref().unwrap_or(""));
    let mut contents = env
        .handle
        .block_on(remote.read_dir(&path, depth.unwrap_or(1)))?;

    // Sort directories first
    fn key(f: &PathInfo) -> (u8, &str) {
        match f {
            PathInfo::Dir(DirectoryInfo {
                cpath: relative_name,
            }) => (0, relative_name),
            PathInfo::File(FileInfo { cpath, .. }) => (1, cpath),
        }
    }
    contents.sort_by(|f, g| key(f).cmp(&key(g)));

    for file in contents {
        match file {
            PathInfo::Dir(DirectoryInfo {
                cpath: relative_name,
            }) => {
                writeln!(env.shell.out(), "{}/", relative_name)?;
            }
            PathInfo::File(FileInfo {
                cpath, size, hash, ..
            }) => {
                if details {
                    let short_hash = hex::encode(&hash[..hash.len().min(4)]);
                    writeln!(env.shell.out(), "{cpath} [{size} b, Hash: {short_hash}]")?;
                } else {
                    writeln!(env.shell.out(), "{cpath}")?;
                }
            }
        }
    }

    Ok(())
}
