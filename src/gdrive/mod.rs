#![allow(unused)]
use client::GDriveClient;
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

pub mod config;
mod client;
mod service;
mod test;

pub use config::{load, init};

use crate::file_repository::AsyncReader;
use crate::file_repository::{FileRepository, Error, Result};
use crate::filetree::FileTree;
use crate::paths::RPath;
use crate::FileInfo;
use crate::PathInfo;

#[derive(Clone, Debug)]
pub struct GDriveRepo {
    inner: Arc<GDriveClient>,
}

impl FileRepository for GDriveRepo {
    type Reader = Pin<Box<dyn AsyncRead + Send>>;

    const CONCURRENCY_LIMIT: usize = client::CONCURRENCY_LIMIT;

    delegate! {
        to self.inner {
            async fn list_files(
                &self,
                filter: &RPath,
                depth: usize,
            ) -> Result<Vec<PathInfo>>;

            async fn delete(&self, path: &RPath) -> Result<()>;

            async fn update_file<R: AsyncReader>(
                &self,
                path: &RPath,
                src: R,
                len: u64,
            ) -> Result<FileInfo>;

            async fn create_file<R: AsyncReader>(
                &self,
                path: &RPath,
                src: R,
                len: u64
            ) -> Result<FileInfo>;

            async fn set_modified_time(
                &self,
                path: &RPath,
                time: time::OffsetDateTime,
            ) -> Result<()>;

            async fn mkdir(&self, path: &RPath) -> Result<()>;

            async fn metadata(&self, path: &RPath) -> Result<PathInfo>;
        }
    }

    async fn read_file(&self, path: &RPath) -> Result<(FileInfo, Self::Reader)> {
        let (info, reader) = self.inner.read_file(path).await?;
        Ok((info, Box::into_pin(reader)))
    }


}
