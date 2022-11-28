mod drive_api;
mod credential;

pub use credential::{GoogleCredential, FatAccessToken};

use crate::{
    append_to_relative_name, backend::FileRepository, gdrive::drive_api::DriveApi,
    split_relative_name, DirectoryInfo, Error, FileInfo, PathInfo,
};
use std::{cell::RefCell, collections::HashMap};
use ureq::Agent;

use self::{drive_api::FilesReq};

#[derive(Debug)]
pub struct GDriveBackend {
    agent: Agent,

    /// Google drive does not use paths. Instead, it identifies files using ids.
    /// For example, there is no specific API to list all of the files in a directory (by path).
    /// If we know the id of a certain path, we therefore cache it here.
    /// For this, we assume that this object is generally short lived and pretend
    /// like concurrent modification is not an issue (someone could move the folder,
    /// for example. Then the cached ID would point to another path now.).
    id_cache: RefCell<HashMap<String, String>>,

    credential: GoogleCredential,
}

impl GDriveBackend {
    const ROOT: &'static str = "root";

    pub fn new(credential: GoogleCredential) -> Self {
        Self {
            credential: credential,
            agent: Agent::new(),
            id_cache: Default::default()
        }
    }

    fn cache_id(&self, relative_name: &str, id: &str) {
        self.id_cache
            .borrow_mut()
            .insert(relative_name.to_string(), id.to_string());
    }

    fn make_drive_api(&self) -> crate::Result<DriveApi> {
        Ok(DriveApi::new(
            self.agent.clone(), 
            &self.credential
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
        if relative_name == "" {
            return Ok(Self::ROOT.to_string());
        }

        // Check if its cached
        if use_cache {
            if let Some(x) = self.id_cache.borrow().get(relative_name) {
                return Ok(x.clone());
            }
        }

        // If not, find our parent, get its id, list its contents. We should be in that list.
        let (parent_relative_name, _) = split_relative_name(relative_name);
        let parent_id = self.get_drive_id_advanced(parent_relative_name, use_cache, only_folders)?;

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
            self.cache_id(&this_relative_name, &file.id);

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
            self.cache_id(relative_name, &ret);
            Ok(ret)
        } else {
            println!("Could not find {}", relative_name);
            println!("heres our cache {:?}", self.id_cache);
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
            return Ok(Self::ROOT.to_string());
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

                let id = self.make_drive_api()?.files_create(req)?;
                self.cache_id(relative_name, &id);

                Ok(id)
            }
            // We handle this case explicitely for readability only.
            Err(Error::NotAFolder(rel)) => return Err(Error::NotAFolder(rel)),
            Err(err) => return Err(err),
        }
    }
}

impl <'a> FileRepository for GDriveBackend {
    fn read_dir(&self, relative_name: &str) -> crate::Result<Vec<crate::PathInfo>> {
        let id = self.get_drive_id(relative_name)?;

        let files = self
            .make_drive_api()?
            .file_list_all(&format!("'{id}' in parents and trashed=false"))?;
        let ret = files
            .into_iter()
            .map(|x| {
                let rel = append_to_relative_name(relative_name, &x.name);
                match (x.size, x.sha1_checksum) {
                    (Some(size), Some(sha1)) => PathInfo::File(crate::FileInfo {
                        relative_name: rel,
                        size,
                        modified: x.modified_time,
                        hash: Some(sha1),
                    }),
                    _ => PathInfo::Dir(crate::DirectoryInfo { relative_name: rel }),
                }
            })
            .collect::<Vec<_>>();

        Ok(ret)
    }

    fn read_file(&self, relative_name: &str, dst: &mut Vec<u8>) -> crate::Result<()> {
        let id = self.get_drive_id(relative_name)?;
        self.make_drive_api()?.files_get_alt_media(&id, None, dst)?;

        Ok(())
    }

    fn write_file(&self, relative_name: &str, src: &[u8]) -> crate::Result<()> {
        match self.get_drive_id(relative_name) {
            Ok(id) => {
                // The file already exists, update it...
                self.make_drive_api()?.files_update_upload(&id, src)?;
            }
            Err(Error::NotFound(_)) => {
                // We need to create the file. For that, we need to place it in the right path
                let (parent, filename) = split_relative_name(relative_name);
                let parent_id = self.mkdir_all(parent)?;

                let id = self.make_drive_api()?.files_create_upload(
                    FilesReq {
                        name: Some(filename.to_string()),
                        parents: Some(vec![parent_id]),
                        ..Default::default()
                    },
                    src,
                )?;
                self.cache_id(relative_name, &id);
            }
            Err(err) => return Err(err),
        }

        Ok(())
    }

    fn delete_file(&self, relative_name: &str) -> crate::Result<()> {
        let id = self.get_drive_id(relative_name)?;
        self.make_drive_api()?.files_delete(&id)?;

        // Try not to invalidate our own cache
        self.id_cache.borrow_mut().remove(relative_name);

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
}
