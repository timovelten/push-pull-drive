use crate::{
    append_to_relative_name, context::Context, file_repository::FileRepository, DirectoryInfo,
    FileInfo, PathInfo,
};

#[derive(clap::Args)]
pub struct LsArgs {
    path: Option<String>,

    #[arg(short, long)]
    depth: Option<u32>,
}

pub async fn remote_ls(ctx: &impl Context, LsArgs { path, depth }: LsArgs) -> anyhow::Result<()> {
    let path = append_to_relative_name(ctx.wd(), path.as_deref().unwrap_or(""));
    let mut contents = ctx.remote().read_dir(&path, depth.unwrap_or(1)).await?;

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
                println!("{}/", relative_name);
            }
            PathInfo::File(FileInfo {
                cpath, size, hash, ..
            }) => {
                let short_hash = hex::encode(&hash[..hash.len().min(4)]);
                println!("{cpath} [{size} b, Hash: {short_hash}]")
            }
        }
    }

    Ok(())
}
