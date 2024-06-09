use crate::{
    context::{Context, Ctx}, db::{Side, TrackedFilesRepository}, file_repository::FileRepository, paths::RPath, util::Shell, FileChange, FileInfo, PathInfo
};
use std::io::Write;
use std::{collections::HashMap, time::Duration};

#[derive(clap::Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub ignore_index: bool,
}

pub fn local_status(ctx: &impl Context, args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, local, .. } = ctx.get();
    let status_of_dir_future = status_of_dir(
        env.tracked_files_or_ignore(args.ignore_index),
        local,
        env.wd(),
        Side::Local,
    );
    env.shell.print_transient("Reading directories...")?;
    let (_, report) = env.handle.block_on(status_of_dir_future)?;

    print_status(&env.shell, &report);
    Ok(())
}

pub fn remote_status(ctx: &impl Context, args: StatusArgs) -> anyhow::Result<()> {
    let Ctx { env, remote, .. } = ctx.get();
    let status_of_dir_future = status_of_dir(
        env.tracked_files_or_ignore(args.ignore_index),
        remote,
        env.wd(),
        Side::Remote,
    );
    env.shell.print_transient("Reading directories...")?;
    let (_, report) = env.handle.block_on(status_of_dir_future)?;

    print_status(&env.shell, &report);
    Ok(())
}

/// Return a list of all files in that directory, and how they have changed from the index.
/// Note that if the target directory does not exist, this simply returns two empty vectors (and NOT
/// an error.)
/// This is also used by push / pull.
pub(super) async fn status_of_dir(
    db: &(impl TrackedFilesRepository + ?Sized),
    repo: &impl FileRepository,
    dir: &RPath,
    side: Side,
) -> anyhow::Result<(Vec<PathInfo>, Vec<FileChange>)> {
    let mut ret: Vec<FileChange> = Vec::new();
    log::debug!("Determining status directory {dir}");

    let all_tracked_files_vec = db.get_all(dir, side)?;

    // List of all tracked files. Mutable so that we can delete "accounted for" files.
    // The ones that remain at the are files that we deleted.
    let mut all_tracked_files = all_tracked_files_vec
        .into_iter()
        .map(|info: FileInfo| (info.rpath.clone(), info))
        .collect::<HashMap<_, _>>();

    // Swallow NotFound errors...
    let all_files_in_repo = match repo.read_dir(dir, u32::MAX).await {
        Ok(res) => res,
        Err(crate::file_repository::Error::NotFound { rpath }) if rpath.as_rpath() == dir => {
            return Ok(Default::default())
        }
        Err(err) => return Err(err.into()),
    };

    for file in &all_files_in_repo {
        if let PathInfo::File(info) = file {
            let tracked_file = all_tracked_files.remove(&info.rpath);
            match tracked_file {
                Some(tracked_info) if is_changed(&tracked_info, info) => {
                    ret.push(FileChange::Modified(info.clone()));
                }
                Some(_) => {
                    // File is tracked but unchanged
                }
                None => {
                    // File is untracked, i.e. new.
                    ret.push(FileChange::New(info.clone()));
                }
            }
        }
    }

    // The files remaining in all_tracked_files were deleted.
    for (_, info) in all_tracked_files {
        ret.push(FileChange::Deleted { rpath: info.rpath })
    }

    Ok((all_files_in_repo, ret))
}

/// Determine if we should a file 'changed' based on an old, and a current `FileInfo`
fn is_changed(old: &FileInfo, new: &FileInfo) -> bool {
    const EPSILON: Duration = Duration::from_millis(10);
    assert_eq!(old.rpath, new.rpath);

    // Size has changed, so the file has definitely changed.
    if old.size != new.size {
        log::trace!("{} is changed due to size: {} --> {}", old.rpath, old.size, new.size);
        return true;
    }

    // Modified date is more recent. Note that we require it to be at least
    // 10 Milliseconds more recent (Filesystems do not store nanosecond precision
    // timestamps)
    if new.modified - old.modified >= EPSILON {
        log::trace!("{} is changed due to modified time: {} --> {}", old.rpath, old.modified, new.modified);
        return true;
    }

    // We use an empty Vec to indicate missing hashes, so if either one is empty,
    // we cant draw any information from the hashes
    if !old.hash.is_empty() && !new.hash.is_empty() && old.hash != new.hash {
        log::trace!("{} is changed due to hash: {:x?} --> {:x?}", old.rpath, old.hash, new.hash);
        return true;
    }

    // Otherwise, we consider the file unchanged.
    false
}

fn print_status(shell: &Shell, changes: &[FileChange]) {
    for change in changes {
        let _ = match change {
            FileChange::New(info) => writeln!(shell.err(), "New: {}", info.rpath),
            FileChange::Modified(info) => writeln!(shell.err(), "Modified: {}", info.rpath),
            FileChange::Deleted { rpath } => writeln!(shell.err(), "Deleted: {}", rpath),
        };
    }
}
