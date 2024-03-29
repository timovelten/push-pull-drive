use crate::{
    append_to_relative_name,
    context::{Ctx, FullContext},
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

pub async fn remote_ls(
    ctx: &impl Ctx,
    LsArgs {
        path,
        depth,
        details,
    }: LsArgs,
) -> anyhow::Result<()> {
    let FullContext { ctx, remote, .. } = ctx.get();

    ctx.shell.print_transient("Reading directory...")?;

    let path = append_to_relative_name(ctx.wd(), path.as_deref().unwrap_or(""));
    let mut contents = remote.read_dir(&path, depth.unwrap_or(1)).await?;

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
                writeln!(ctx.shell.out(), "{}/", relative_name)?;
            }
            PathInfo::File(FileInfo {
                cpath, size, hash, ..
            }) => {
                if details {
                    let short_hash = hex::encode(&hash[..hash.len().min(4)]);
                    writeln!(ctx.shell.out(), "{cpath} [{size} b, Hash: {short_hash}]")?;
                } else {
                    writeln!(ctx.shell.out(), "{cpath}")?;
                }
            }
        }
    }

    Ok(())
}
