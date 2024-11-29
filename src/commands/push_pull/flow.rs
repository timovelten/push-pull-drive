use anyhow::bail;

use super::PushPullArgs;
use crate::filetree::{self, Node, Patch, PatchKind};
use crate::paths::RPath;
use crate::{context::ExecutionEnvironment, file_repository::FileRepository};
use crate::PathInfo;
use std::collections::VecDeque;
use std::io::Write as _;
use std::time::Duration;

#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct PushPullFlow<A, B> {
    args: PushPullArgs,
    ctx: ExecutionEnvironment,
    from: A,
    to: B,
}

impl<A, B> PushPullFlow<A, B>
where
    A: FileRepository,
    B: FileRepository,
{
    pub(super) fn new_pull(
        local: B,
        remote: A,
        args: PushPullArgs,
        ctx: ExecutionEnvironment,
    ) -> Self {
        Self {
            args,
            ctx,
            from: remote,
            to: local,
        }
    }

    pub(super) fn new_push(
        local: A,
        remote: B,
        args: PushPullArgs,
        ctx: ExecutionEnvironment,
    ) -> Self {
        Self {
            args,
            ctx,
            from: local,
            to: remote,
        }
    }

    fn stderr(&self) -> impl std::io::Write + '_ {
        self.ctx.shell.err()
    }

    pub(super) fn execute(self) -> anyhow::Result<()> {
        let handle = self.ctx.handle.clone();
        handle.block_on(self.execute_async())
    }

    /// Try and do everything a patch wants us to do, except actually transfering files.
    ///
    /// Note that this does not check if the file is actually modified. We leave that
    /// to somewhere else
    async fn prepare_patch<'a>(
        &self,
        patch: &'a Patch<'_>,
        to_be_updated: &mut Vec<&'a RPath>,
        to_be_created: &mut Vec<&'a RPath>,
    ) -> anyhow::Result<()> {
        match patch.kind {
            PatchKind::Create { terminal: true } => {
                // Creating a file is done by someone eles
                to_be_created.push(&patch.path);
            }
            PatchKind::Create { terminal: false } => {
                self.to.mkdir(&patch.path).await?;
            }
            PatchKind::Delete { .. } => {
                self.to.delete(&patch.path).await?;
            }
            PatchKind::Unchanged { terminal: true } => {
                // If needed, file updates are handled by someone else.
                to_be_updated.push(&patch.path);
            }
            PatchKind::Unchanged { terminal: false } => {
                // Directory updates are unimportant to us
            }
            PatchKind::Replace { terminal: true } => {
                // Replace a dir by a file
                self.to.delete(&patch.path).await?;
                to_be_created.push(&patch.path);
            }
            PatchKind::Replace { terminal: false } => {
                // Replace a file by a directory
                self.to.delete(&patch.path).await?;
                self.to.mkdir(&patch.path).await?;
            }
        }

        Ok(())
    }

    async fn transfer_file(
        &self,
        path: &RPath,
    ) -> anyhow::Result<()> {
        let (info, reader) = self.from.read_file(path).await?;
        let info = self
            .to
            .write_file(path, Some(info.modified), reader, info.size)
            .await?;

        writeln!(self.stderr(), "Wrote file {info:?} info")?;

        Ok(())
    }

    async fn execute_async(self) -> anyhow::Result<()> {
        let path = &self.ctx.wd;

        let from_files = self.from.file_tree(path, usize::MAX).await?;
        let to_files = self.to.file_tree(path, usize::MAX).await?;

        let patches = filetree::diff(&to_files, &from_files);

        let mut failures = Vec::new();
        let mut to_be_created = Vec::new();
        let mut to_be_updated = Vec::new();

        // Go through what we have to do in BFS order, skipping over entire branches
        // if a parent operation fails.
        let mut queue: VecDeque<&RPath> = vec![RPath::empty()].into();
        while let Some(path) = queue.pop_front() {
            let patch = patches.get(path).expect("path is a node in the tree");
            debug_assert_eq!(patch.path, path);

            // Execute the operation associated with this patch
            match self
                .prepare_patch(patch, &mut to_be_updated, &mut to_be_created)
                .await
            {
                Ok(b) => b,
                Err(err) => {
                    failures.push((patch, err));
                    continue;
                }
            };

            let children = patches
                .children(path)
                .expect("path is a node in the tree")
                .map(|x| x.path())
                .collect::<Vec<_>>();

            queue.extend(children);
        }

        // Prune the files that need to be updated, leaving out the ones that are up to date
        let to_be_updated = to_be_updated
            .into_iter()
            .filter(|path| {
                let from_info = from_files.get(path).unwrap().unwrap_fileinfo();
                let to_info = to_files.get(path).unwrap().unwrap_fileinfo();

                let unchanged = from_info.size == to_info.size
                    && (from_info.modified - to_info.modified) < Duration::from_secs(1);

                if unchanged {
                    let _ = writeln!(self.stderr(), "{path:?} is up to date");
                }
                !unchanged
            })
            .collect::<Vec<_>>();

        let to_be_transferred = to_be_updated.into_iter().chain(to_be_created.into_iter());

        // Now, go ahead and transfer files
        for file in to_be_transferred {
            if let Err(err) = self
                .transfer_file(file)
                .await
            {
                writeln!(self.stderr(), "Transferring file {file:?} failed: {err}")?;
            }
        }

        Ok(())
    }
}

// #[cfg(test)]
// mod test {
//     use super::*;
//     use crate::FileInfo;
//     use time::OffsetDateTime;

//     fn f(s: &str) -> FileInfo {
//         FileInfo {
//             rpath: RPathBuf::from_sep(s, '/'),
//             size: 42,
//             modified: OffsetDateTime::now_utc(),
//             hash: Default::default(),
//         }
//     }

//     fn test_changes_1() -> Vec<FileChange> {
//         // When changing this, it is important to take care not to invalidate all the test that use this.
//         let mut ret = vec![
//             FileChange::New(f("documents/2024/invoice")),
//             FileChange::New(f("documents/2024/letter")),
//             FileChange::Modified(f("documents/2024/cv")),
//             FileChange::Deleted {
//                 rpath: ["documents", "template"].into(),
//             },
//             FileChange::New(f("videos/test/vid.mp4")),
//             FileChange::New(f("videos/test/vid2.mkv")),
//             FileChange::Modified(f("videos/test/another.mkv")),
//             FileChange::Deleted {
//                 rpath: ["tmp", "file1"].into(),
//             },
//             FileChange::Deleted {
//                 rpath: ["tmp", "file2"].into(),
//             },
//         ];
//         sort_filechanges(&mut ret);

//         ret
//     }

//     #[test]
//     fn deletion_should_not_conflict_with_modification() {
//         let changes = test_changes_1();

//         let res = either_okay_or_conflict(FileChange::Modified(f("documents/template")), &changes);
//         assert!(matches!(res, Either::Left(_)));
//     }

//     #[test]
//     fn conflict_detection_both_modified() {
//         let to_changes = test_changes_1();

//         let Either::Right(Conflict::BothModified { dst, .. }) = either_okay_or_conflict(
//             FileChange::Modified(f("documents/2024/letter")),
//             &to_changes,
//         ) else {
//             panic!();
//         };
//         assert_eq!(
//             dst.rpath(),
//             &*RPathBuf::from(["documents", "2024", "letter"])
//         );
//     }

//     #[test]
//     fn conflict_target_is_dir() {
//         let changes = test_changes_1();

//         let info = f("videos/test");
//         for change in [FileChange::Modified(info.clone()), FileChange::New(info)] {
//             let Either::Right(Conflict::IsADirectory { contents, .. }) =
//                 either_okay_or_conflict(change, &changes)
//             else {
//                 panic!();
//             };
//             assert_eq!(contents.len(), 3);
//         }
//     }

//     #[test]
//     fn conflict_target_is_empty_dir() {
//         let changes = test_changes_1();

//         // This one a is a bit tricky. Clearly, the user was able to create the file "tmp" (even though that
//         // was a folder at the point in time we last synced). Unless there were remote changes in that folder
//         // the user knowingly deleted the folder, and replaced it by a file. There is no conflict here!
//         let Either::Left(_) = either_okay_or_conflict(FileChange::Modified(f("tmp")), &changes)
//         else {
//             panic!();
//         };
//     }

//     #[test]
//     fn conflict_subpath_is_file() {
//         let changes = test_changes_1();

//         let Either::Right(Conflict::SubpathIsFile { file, .. }) = either_okay_or_conflict(
//             FileChange::Modified(f("documents/2024/cv/pictures/coverpic.jpg")),
//             &changes,
//         ) else {
//             panic!()
//         };
//         assert_eq!(file.rpath(), &*RPathBuf::from(["documents", "2024", "cv"]));
//     }
// }
