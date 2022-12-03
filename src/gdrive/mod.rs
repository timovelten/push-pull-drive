mod credential;
mod drive_api;
pub mod oauth;
mod extensions;

pub use credential::{FatAccessToken, GoogleCredential};
use isahc::{HttpClient};

use crate::{
    append_to_relative_name, backend::FileRepository, gdrive::drive_api::DriveApi,
    split_relative_name, DirectoryInfo, Error, FileInfo, PathInfo,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex}
};

use self::drive_api::FilesReq;

#[derive(Debug)]
pub struct GDriveBackend {
    client: HttpClient,

    /// Google drive does not use paths. Instead, it identifies files using ids.
    /// For example, there is no specific API to list all of the files in a directory (by path).
    /// If we know the id of a certain path, we therefore cache it here.
    /// For this, we assume that this object is generally short lived and pretend
    /// like concurrent modification is not an issue (someone could move the folder,
    /// for example. Then the cached ID would point to another path now.).
    id_cache: Arc<IdCache>,
    credential: Arc<GoogleCredential>,
}

impl GDriveBackend {
    pub fn new(credential: GoogleCredential) -> Self {
        let client = HttpClient::builder()
            .max_connections_per_host(10)
            .build()
            .unwrap();

        Self {
            credential: Arc::new(credential),
            client,
            id_cache: Default::default(),
        }
    }

    fn make_drive_api(&self) -> crate::Result<DriveApi> {
        Ok(DriveApi::new(
            self.client.clone(),
            self.credential.get_or_refresh_token()?,
        ))
    }

    /// Try and find the Google-Drive-ID of the given relative name (path). If use_cache is set,
    /// the results may come from a cache. This cache is never invalidated for the lifetime
    /// of the object Self, since it is assumed that this object is generally short lived.
    /// If only_folders is set, this returns the Error::NotAFolder if *something* exists at the given
    /// relative path, but it is not a folder.
    fn get_drive_id_advanced(
        &self,
        relative_name: &str,
        use_cache: bool,
        only_folders: bool,
    ) -> crate::Result<String> {
        // Check if its cached
        if use_cache {
            if let Some(x) = self.id_cache.get(relative_name) {
                return Ok(x.clone());
            }
        }

        // Are we looking for root? root also has an Id and we need to find it,
        // because we use these ids to determine paths in read dirs.
        if relative_name == "" {
            let f = self.make_drive_api()?.files_get("root")?;
            debug_assert_eq!(f.parents, None);

            self.id_cache.update(relative_name, &f.id);
            return Ok(f.id);
        }

        // If not, find our parent, get its id, list its contents. We should be in that list.
        let (parent_relative_name, _) = split_relative_name(relative_name);
        let parent_id =
            self.get_drive_id_advanced(parent_relative_name, use_cache, only_folders)?;

        // It would be possible here to only list the folders first, and check if
        // we are looking for a folder. This would only really be worth it when one assumes that
        // there are far less folders in a folder than files. I am unsure if that is the case in
        // general or if thats worth it.
        let mut ret = None;

        let files = self
            .make_drive_api()?
            .file_list_all(&format!("'{parent_id}' in parents and trashed=false"))?;
        for file in files {
            let this_relative_name = append_to_relative_name(parent_relative_name, &file.name);
            // Use the opportunity to remember a lot of the ids
            self.id_cache.update(&this_relative_name, &file.id);

            if this_relative_name == relative_name {
                if ret.is_some() {
                    println!(
                        "Warning: Folder {} [{}] contains two children with the same name",
                        parent_relative_name, parent_id
                    );
                }

                if only_folders && !file.is_folder() {
                    return Err(Error::NotAFolder(relative_name.to_string()));
                }

                ret = Some(file.id)
            }
        }

        if let Some(ret) = ret {
            self.id_cache.update(relative_name, &ret);
            Ok(ret)
        } else {
            Err(Error::NotFound(relative_name.to_string()))
        }
    }

    fn get_drive_id(&self, relative_name: &str) -> crate::Result<String> {
        self.get_drive_id_advanced(relative_name, true, false)
    }

    /// Creates the given relative name and returns its id. If this path
    /// already exists and is the path of a file (and not a folder) this returns
    /// `Error::NotAFolder`.
    fn mkdir_all(&self, relative_name: &str) -> crate::Result<String> {
        // Base case: root does not need to be created
        if relative_name == "" {
            return self.get_drive_id(relative_name);
        }

        // Base case: If this already exists, just return its id
        match self.get_drive_id_advanced(relative_name, true, true) {
            Ok(id) => return Ok(id),
            Err(Error::NotFound(_)) => {
                // Path does not yet exist, so we find our parent and create a new folder there.
                let (parent, name) = split_relative_name(relative_name);

                let parent_id = self.mkdir_all(parent)?;
                let req = drive_api::FilesReq {
                    name: Some(name.to_string()),
                    mime_type: Some("application/vnd.google-apps.folder".to_string()),
                    parents: Some(vec![parent_id]),
                    ..Default::default()
                };

                let id = self.make_drive_api()?.files_create(req)?.id;
                self.id_cache.update(relative_name, &id);

                Ok(id)
            }
            // We handle this case explicitely for readability only.
            Err(Error::NotAFolder(rel)) => return Err(Error::NotAFolder(rel)),
            Err(err) => return Err(err),
        }
    }
}

impl<'a> FileRepository for GDriveBackend {
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<crate::PathInfo>> {
        let id = self.get_drive_id(relative_name)?;

        let files = self
            .make_drive_api()?
            .file_list_all(&format!("'{id}' in parents and trashed=false"))?;
        let ret = files
            .into_iter()
            .filter_map(|x| {
                let rel = append_to_relative_name(relative_name, &x.name);
                match (x.size, x.sha1_checksum, x.mime_type.as_str()) {
                    (Some(size), Some(sha1), _) => Some(PathInfo::File(crate::FileInfo {
                        relative_name: rel,
                        size,
                        modified: x.modified_time,
                        hash: Some(sha1),
                    })),
                    (_, _, "application/vnd.google-apps.folder") => {
                        Some(PathInfo::Dir(crate::DirectoryInfo { relative_name: rel }))
                    }
                    (_, _, _) => {
                        // Skip types we cannot cateogorize into file / folder
                        None
                    }
                }
            })
            .collect::<Vec<_>>();

        Ok(ret)
    }

    fn read_file(&self, relative_name: &str) -> crate::Result<Vec<u8>> {
        let id = self.get_drive_id(relative_name)?;
        let mut body = self.make_drive_api()?.files_get_alt_media(&id, None)?;

        let mut ret = Vec::new();
        std::io::copy(&mut body, &mut ret)?;

        Ok(ret)
    }

    fn write_file(&self, relative_name: &str, src: Arc<[u8]>) -> crate::Result<()> {
        let src = src.as_ref();

        match self.get_drive_id(relative_name) {
            Ok(id) => {
                // The file already exists, update it...
                self.make_drive_api()?.files_update_upload(&id, src)?;
            }
            Err(Error::NotFound(_)) => {
                // We need to create the file. For that, we need to place it in the right path
                let (parent, filename) = split_relative_name(relative_name);
                let parent_id = self.mkdir_all(parent)?;

                let id = self
                    .make_drive_api()?
                    .files_create_upload(
                        FilesReq {
                            name: Some(filename.to_string()),
                            parents: Some(vec![parent_id]),
                            ..Default::default()
                        },
                        src,
                    )?
                    .id;

                self.id_cache.update(relative_name, &id);
            }
            Err(err) => return Err(err),
        }

        Ok(())
    }

    fn delete_file(&self, relative_name: &str) -> crate::Result<()> {
        let id = self.get_drive_id(relative_name)?;
        self.make_drive_api()?.files_delete(&id)?;

        // Try not to invalidate our own cache
        self.id_cache.delete(relative_name);

        Ok(())
    }

    fn set_modified_time(
        &self,
        relative_name: &str,
        modtime: time::OffsetDateTime,
    ) -> crate::Result<()> {
        let id = self.get_drive_id(relative_name)?;

        self.make_drive_api()?.files_update(
            &id,
            FilesReq {
                modified_time: Some(modtime),
                ..Default::default()
            },
        )?;

        Ok(())
    }

    fn info(&self, relative_name: &str) -> crate::Result<crate::PathInfo> {
        let id = self.get_drive_id(relative_name)?;

        let metadata = self.make_drive_api()?.files_get(&id)?;

        let ret = if let Some(size) = metadata.size {
            PathInfo::File(FileInfo {
                relative_name: relative_name.to_string(),
                modified: metadata.modified_time,
                size: size,
                hash: metadata.sha1_checksum,
            })
        } else {
            PathInfo::Dir(DirectoryInfo {
                relative_name: relative_name.to_string(),
            })
        };

        Ok(ret)
    }

    fn read_dirs(&self, dirs: Vec<String>) -> crate::Result<Vec<PathInfo>> {
        const BATCH_SIZE: usize = 8;

        let mut ret = Vec::new();

        // ID's seem to be no longer than 50 characters. We batch them in groups of 8 for now.
        // That also has the advantage that we do not have to allocate at all here (for a reverse
        // lookup table).
        let mut ids = Vec::new();
        for rns_chunk in dirs.chunks(BATCH_SIZE) {
            ids.clear();
            for i in 0..rns_chunk.len() {
                ids.insert(i, self.get_drive_id(rns_chunk[i].as_ref())?);
            }

            let mut q = ids
                .iter()
                .map(|id| format!("'{id}' in parents"))
                .collect::<Vec<String>>()
                .join(" or ");

            q = format!("({q}) and trashed = false");

            let files = self.make_drive_api()?.file_list_all(&q)?;

            for file in files {
                let parent_id = match file.parents {
                    Some([x]) => x,
                    _ => panic!("unexpectedly got file without parents"),
                };

                let parent_relative_name_idx = ids
                    .iter()
                    .position(|id| id == &parent_id)
                    .expect("failed to find path of parent");

                let relative_name = append_to_relative_name(
                    rns_chunk[parent_relative_name_idx].as_ref(),
                    &file.name,
                );

                let res = match (file.size, file.sha1_checksum, file.mime_type.as_str()) {
                    (Some(size), Some(sha1), _) => Some(PathInfo::File(crate::FileInfo {
                        relative_name,
                        size,
                        modified: file.modified_time,
                        hash: Some(sha1),
                    })),
                    (_, _, "application/vnd.google-apps.folder") => {
                        Some(PathInfo::Dir(crate::DirectoryInfo { relative_name }))
                    }
                    (_, _, _) => {
                        // Skip types we cannot cateogorize into file / folder
                        // FIXME: Do something for Google docs here
                        None
                    }
                };

                if let Some(res) = res {
                    ret.push(res);
                }
            }
        }

        Ok(ret)
    }
}

#[derive(Default, Debug)]
struct IdCache {
    inner: Mutex<HashMap<String, String>>,
}

impl IdCache {
    pub fn update<K: ToString, V: ToString>(&self, key: K, val: V) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), val.to_string());
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().unwrap().get(key).map(|x| x.to_owned())
    }

    pub fn delete(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_sync<T: Send>() {}
    const fn assert_clone<T: Clone>() {}

    assert_send::<IdCache>();
    assert_sync::<IdCache>();

    assert_send::<GDriveBackend>();
};
