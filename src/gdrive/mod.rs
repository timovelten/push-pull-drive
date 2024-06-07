#![allow(unused)]

mod client;
pub mod config;
mod credential;
mod drive_api;
mod extensions;
pub mod oauth;

use self::drive_api::File;
use self::drive_api::FileId;
use self::drive_api::FilesReq;
use crate::append_to_cpath_in_place;
use crate::file_repository::AsyncReader;
use crate::file_repository::Error;
use crate::file_repository::Result;
use crate::gdrive::drive_api::MoreFileFields;
use crate::paths::filename;
use crate::paths::subpaths;
use crate::util::hex_must_decode;
use crate::DirectoryInfo;
use crate::{
    append_to_relative_name, file_repository::FileRepository, gdrive::drive_api::DriveApi,
    pop_cpath, FileInfo, PathInfo,
};
use anyhow::Context;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use cached::Cached;
use cached::TimedCache;
use cached::TimedSizedCache;
use client::GDriveClient;
pub use config::*;
pub use credential::{FatAccessToken, GoogleCredential};
use delegate::delegate;
use futures::stream::FuturesUnordered;
use futures::stream::Next;
use futures::AsyncRead;
use futures::StreamExt;
use std::collections::VecDeque;
use std::fmt::Write;
use std::ops::Deref;
use std::pin::Pin;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::{sync::Semaphore, task::block_in_place};

#[derive(Clone, Debug)]
pub struct GDriveRepo {
    inner: Arc<GDriveClient>,
}

impl FileRepository for GDriveRepo {
    type Reader = Pin<Box<dyn AsyncRead + Send>>;

    /// Google does not really like too many concurrent connections. In my personal testing, the limit seemed to be somewhere below 10.
    const CONCURRENCY_LIMIT: usize = 8;

    delegate! {
        to self.inner {
            async fn read_dir(&self, cpath: &str, up_to_depth: u32) -> Result<Vec<PathInfo>>;

            async fn get_metadata(&self, cpath: &str) -> Result<PathInfo>;

            async fn delete_file(&self, cpath: &str) -> Result<()>;

            async fn set_modified_time(&self, cpath: &str, modtime: time::OffsetDateTime) -> Result<()>;

            async fn write_file<R: AsyncReader>(
                &self,
                cpath: &str,
                modified_time: Option<time::OffsetDateTime>,
                src: R,
                len: u64,
            ) -> Result<FileInfo>;
        }
    }

    async fn read_file(&self, cpath: &str) -> Result<(FileInfo, Self::Reader)> {
        let (info, reader) = self.inner.read_file(cpath).await?;
        Ok((info, Box::into_pin(reader)))
    }
}
