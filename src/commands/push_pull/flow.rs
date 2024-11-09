use super::PushPullArgs;
use crate::db::{SqliteDatabase, Tracked};
use crate::file_repository::Change;
use crate::{context::ExecutionEnvironment, file_repository::FileRepository, FileChange};
use std::io::Write as _;

#[derive(Clone)]
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

    fn db(&self) -> &SqliteDatabase {
        &self.ctx.db
    }

    fn stderr(&self) -> impl std::io::Write + '_ {
        self.ctx.shell.err()
    }

    pub(super) fn execute(self) -> anyhow::Result<()> {
        let handle = self.ctx.handle.clone();
        handle.block_on(self.execute_async())
    }

    async fn execute_async(self) -> anyhow::Result<()> {
        todo!()
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
