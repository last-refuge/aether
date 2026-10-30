//! Catalogs the browser can show: sample files, or a signed-in Drive's local cache.

use std::path::PathBuf;

use aerie_core::{Catalog, DriveFileId, FileEntry, MemoryCatalog};
use aerie_store::Store;

/// Reads a signed-in account's cached metadata. The engine writes the same database on
/// its own connection; WAL mode lets this one read without waiting.
pub struct StoreCatalog {
    store: Store,
    account: String,
}

impl StoreCatalog {
    pub fn open(account: String) -> aerie_store::Result<Self> {
        Ok(Self {
            store: Store::open(&Store::default_path())?,
            account,
        })
    }

    pub fn root(&self) -> Option<DriveFileId> {
        self.store.root_id(&self.account).ok().flatten()
    }

    /// My Drive as a folder that can be synced.
    pub fn my_drive(&self) -> Option<FileEntry> {
        let root = self.root()?;
        logged(self.store.sync_root_entry(&self.account, &root))
    }

    pub fn indexing(&self) -> aerie_store::Indexing {
        self.store
            .indexing(&self.account)
            .unwrap_or(aerie_store::Indexing {
                all: true,
                drives: Vec::new(),
            })
    }

    pub fn shared_drives(&self) -> Vec<FileEntry> {
        logged(self.store.shared_drives(&self.account))
    }

    pub fn sync_settings(&self) -> Option<aerie_store::SyncSettings> {
        logged(self.store.sync_settings(&self.account))
    }

    pub fn sync_roots(&self) -> Vec<DriveFileId> {
        logged(self.store.sync_roots(&self.account))
    }

    /// The drive's local directory, including when only folders inside it sync.
    /// Uses the engine's mapping so sanitized and duplicate names stay consistent.
    pub fn local_drive_folder(&self, drive: &DriveFileId) -> Option<PathBuf> {
        let settings = self.sync_settings()?;
        let roots = self.sync_roots();
        for (mut entry, mut path) in
            aerie_sync::cycle::root_locations(&self.store, &self.account, &roots)
        {
            // Bounded, so malformed parent cycles can't hang the UI.
            for _ in 0..256 {
                if &entry.id == drive {
                    return Some(settings.local_root.join(path));
                }
                let Some(parent) = &entry.parent else { break };
                if !path.pop() {
                    break;
                }
                if parent == drive {
                    return Some(settings.local_root.join(path));
                }
                let Some(above) = self.get(parent) else { break };
                entry = above;
            }
        }
        None
    }

    pub fn transfers(&self) -> Vec<aerie_store::Transfer> {
        logged(self.store.transfers(&self.account))
    }

    pub fn inventory_complete(&self) -> bool {
        self.store
            .inventory_complete(&self.account)
            .unwrap_or(false)
    }
}

/// Read failures are logged and shown as nothing, never as deleted files.
fn logged<T: Default>(result: aerie_store::Result<T>) -> T {
    result.unwrap_or_else(|err| {
        tracing::error!(%err, "couldn't read the metadata cache");
        T::default()
    })
}

impl Catalog for StoreCatalog {
    fn children(&self, folder: &DriveFileId) -> Vec<FileEntry> {
        logged(self.store.children(&self.account, folder))
    }

    fn search(&self, query: &str) -> Vec<FileEntry> {
        logged(self.store.search(&self.account, query, 500))
    }

    fn get(&self, id: &DriveFileId) -> Option<FileEntry> {
        logged(self.store.file(&self.account, id))
    }
}

pub enum AppCatalog {
    Samples(MemoryCatalog),
    Drive(StoreCatalog),
}

impl AppCatalog {
    pub fn is_drive(&self) -> bool {
        matches!(self, Self::Drive(_))
    }
}

impl Catalog for AppCatalog {
    fn children(&self, folder: &DriveFileId) -> Vec<FileEntry> {
        match self {
            Self::Samples(catalog) => catalog.children(folder),
            Self::Drive(catalog) => catalog.children(folder),
        }
    }

    fn search(&self, query: &str) -> Vec<FileEntry> {
        match self {
            Self::Samples(catalog) => catalog.search(query),
            Self::Drive(catalog) => catalog.search(query),
        }
    }

    fn get(&self, id: &DriveFileId) -> Option<FileEntry> {
        match self {
            Self::Samples(catalog) => catalog.get(id),
            Self::Drive(catalog) => catalog.get(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerie_core::{Capabilities, FileKind};
    use aerie_store::{Account, SyncSettings};

    fn folder(id: &str, parent: Option<&str>, name: &str) -> FileEntry {
        FileEntry {
            id: DriveFileId::new(id),
            parent: parent.map(DriveFileId::new),
            name: name.into(),
            kind: FileKind::Folder,
            size: None,
            modified: 0,
            mime_type: if parent.is_none() {
                aerie_drive::api::SHARED_DRIVE_MIME.into()
            } else {
                "application/vnd.google-apps.folder".into()
            },
            md5: None,
            head_revision: None,
            drive: None,
            can: Capabilities::default(),
        }
    }

    fn catalog(entries: &[FileEntry], roots: &[&str], paused: bool) -> StoreCatalog {
        let mut store = Store::open_in_memory().unwrap();
        let account = "account";
        store
            .save_account(&Account {
                id: account.into(),
                email: "test@example.com".into(),
                display_name: "Test".into(),
            })
            .unwrap();
        store
            .begin_inventory(account, &DriveFileId::new("my-drive"), "cursor")
            .unwrap();
        store.save_inventory_page(account, entries).unwrap();
        store.finish_inventory(account).unwrap();
        store
            .save_sync_settings(
                account,
                &SyncSettings {
                    local_root: PathBuf::from("/tmp/Sync & files"),
                    paused,
                },
            )
            .unwrap();
        let roots: Vec<_> = roots.iter().map(|id| DriveFileId::new(*id)).collect();
        store.set_sync_roots(account, &roots, &[]).unwrap();
        StoreCatalog {
            store,
            account: account.into(),
        }
    }

    #[test]
    fn locates_whole_my_drive() {
        let catalog = catalog(&[], &["my-drive"], false);
        assert_eq!(
            catalog.local_drive_folder(&DriveFileId::new("my-drive")),
            Some(PathBuf::from("/tmp/Sync & files/My Drive"))
        );
        assert_eq!(catalog.local_drive_folder(&DriveFileId::new("other")), None);
    }

    #[test]
    fn locates_partially_synced_drives_while_paused() {
        let catalog = catalog(
            &[
                folder("projects", Some("my-drive"), "Projects"),
                folder("docs", Some("projects"), "Docs"),
                folder("shared", None, "Team"),
                folder("reports", Some("shared"), "Reports"),
                folder("annual", Some("reports"), "Annual"),
            ],
            &["docs", "annual"],
            true,
        );
        assert_eq!(
            catalog.local_drive_folder(&DriveFileId::new("my-drive")),
            Some(PathBuf::from("/tmp/Sync & files/My Drive"))
        );
        assert_eq!(
            catalog.local_drive_folder(&DriveFileId::new("shared")),
            Some(PathBuf::from("/tmp/Sync & files/Team"))
        );
    }

    #[test]
    fn uses_sanitized_and_numbered_drive_names() {
        let catalog = catalog(
            &[
                folder("first", None, "Team/Design"),
                folder("second", None, "Team/Design"),
            ],
            &["first", "second"],
            false,
        );
        assert_eq!(
            catalog.local_drive_folder(&DriveFileId::new("first")),
            Some(PathBuf::from("/tmp/Sync & files/Team∕Design"))
        );
        assert_eq!(
            catalog.local_drive_folder(&DriveFileId::new("second")),
            Some(PathBuf::from("/tmp/Sync & files/Team∕Design (2)"))
        );
    }

    #[test]
    fn has_no_local_folder_after_sync_is_stopped_or_deselected() {
        let mut catalog = catalog(&[], &["my-drive"], true);
        let drive = DriveFileId::new("my-drive");
        catalog
            .store
            .set_sync_roots(&catalog.account, &[], &[])
            .unwrap();
        assert_eq!(catalog.local_drive_folder(&drive), None);
        catalog
            .store
            .set_sync_roots(&catalog.account, std::slice::from_ref(&drive), &[])
            .unwrap();
        catalog.store.stop_sync(&catalog.account).unwrap();
        assert_eq!(catalog.local_drive_folder(&drive), None);
    }
}
