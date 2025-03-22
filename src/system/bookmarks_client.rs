//! ## BookmarksClient
//!
//! `bookmarks_client` is the module which provides an API between the Bookmarks module and the system

// Crate
// Ext
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::string::ToString;
use std::time::SystemTime;

use super::keys::filestorage::FileStorage;
use super::keys::keyringstorage::KeyringStorage;
use super::keys::{KeyStorage, KeyStorageError};
// Local
use crate::config::{
    bookmarks::{Bookmark, UserHosts},
    serialization::{SerializerError, SerializerErrorKind, deserialize, serialize},
};
use crate::filetransfer::FileTransferParams;
use crate::utils::crypto;
use crate::utils::fmt::fmt_time;
use crate::utils::random::random_alphanumeric_with_len;

/// BookmarksClient provides a layer between the host system and the bookmarks module
pub struct BookmarksClient {
    hosts: UserHosts,
    bookmarks_file: PathBuf,
    key: String,
    recents_size: usize,
}

impl BookmarksClient {
    /// Instantiates a new BookmarksClient
    /// Bookmarks file path must be provided
    /// Storage path for file provider must be provided
    pub fn new(
        bookmarks_file: &Path,
        storage_path: &Path,
        recents_size: usize,
        keyring: bool,
    ) -> Result<BookmarksClient, SerializerError> {
        // Create default hosts
        let default_hosts: UserHosts = UserHosts::default();
        debug!("Setting up bookmarks client...");
        // Get key storage
        let (key_storage, service_id) = Self::keyring(storage_path, keyring);
        // Load key
        let key: String = match key_storage.get_key(service_id) {
            Ok(k) => {
                debug!("Key loaded with success");
                k
            }
            Err(e) => match e {
                KeyStorageError::NoSuchKey => {
                    // If no such key, generate key and set it into the storage
                    let key: String = Self::generate_key();
                    debug!("Key doesn't exist yet or could not be loaded; generated a new key");
                    if let Err(e) = key_storage.set_key(service_id, key.as_str()) {
                        error!("Failed to set new key into storage: {}", e);
                        return Err(SerializerError::new_ex(
                            SerializerErrorKind::Io,
                            format!("Could not write key to storage: {e}"),
                        ));
                    }
                    // Return key
                    key
                }
                _ => {
                    error!("Failed to get key from storage: {}", e);
                    return Err(SerializerError::new_ex(
                        SerializerErrorKind::Io,
                        format!("Could not get key from storage: {e}"),
                    ));
                }
            },
        };
        let mut client: BookmarksClient = BookmarksClient {
            hosts: default_hosts,
            bookmarks_file: PathBuf::from(bookmarks_file),
            key,
            recents_size,
        };
        // If bookmark file doesn't exist, initialize it
        if !bookmarks_file.exists() {
            info!("Bookmarks file doesn't exist yet; creating it...");
            if let Err(err) = client.write_bookmarks() {
                error!("Failed to create bookmarks file: {}", err);
                return Err(err);
            }
        } else {
            // Load bookmarks from file
            if let Err(err) = client.read_bookmarks() {
                error!("Failed to load bookmarks: {}", err);
                return Err(err);
            }
        }
        info!("Bookmarks client initialized");
        // Load key
        Ok(client)
    }

    /// Get the key storage
    fn keyring(storage_path: &Path, keyring: bool) -> (Box<dyn KeyStorage>, &'static str) {
        if keyring && cfg!(feature = "keyring") {
            debug!("Setting up KeyStorage");
            let username: String = whoami::username();
            let storage: KeyringStorage = KeyringStorage::new(username.as_str());
            // Check if keyring storage is supported
            #[cfg(not(test))]
            let app_name: &str = "termscp";
            #[cfg(test)] // NOTE: when running test, add -test
            let app_name: &str = "termscp-test";
            match storage.is_supported() {
                true => {
                    debug!("Using KeyringStorage");
                    (Box::new(storage), app_name)
                }
                false => {
                    warn!("KeyringStorage is not supported; using FileStorage");
                    (Box::new(FileStorage::new(storage_path)), "bookmarks")
                }
            }
        } else {
            #[cfg(not(test))]
            let app_name: &str = "bookmarks";
            #[cfg(test)] // NOTE: when running test, add -test
            let app_name: &str = "bookmarks-test";
            debug!("Using FileStorage");
            (Box::new(FileStorage::new(storage_path)), app_name)
        }
    }

    /// Iterate over bookmarks keys
    pub fn iter_bookmarks(&self) -> impl Iterator<Item = &String> + '_ {
        Box::new(self.hosts.bookmarks.keys())
    }

    /// Get bookmark associated to key
    pub fn get_bookmark(&self, key: &str) -> Option<FileTransferParams> {
        debug!("Getting bookmark {}", key);
        let mut entry: Bookmark = self.hosts.bookmarks.get(key).cloned()?;
        // Decrypt password first
        if let Some(pwd) = entry.password.as_mut() {
            match self.decrypt_str(pwd.as_str()) {
                Ok(decrypted_pwd) => {
                    *pwd = decrypted_pwd;
                }
                Err(err) => {
                    error!("Failed to decrypt `password` for bookmark {}: {}", key, err);
                }
            }
        }
        // Decrypt AWS-S3 params
        if let Some(s3) = entry.s3.as_mut() {
            // Access key
            if let Some(access_key) = s3.access_key.as_mut() {
                match self.decrypt_str(access_key.as_str()) {
                    Ok(plain) => {
                        *access_key = plain;
                    }
                    Err(err) => {
                        error!(
                            "Failed to decrypt `access_key` for bookmark {}: {}",
                            key, err
                        );
                    }
                }
            }
            // Secret access key
            if let Some(secret_access_key) = s3.secret_access_key.as_mut() {
                match self.decrypt_str(secret_access_key.as_str()) {
                    Ok(plain) => {
                        *secret_access_key = plain;
                    }
                    Err(err) => {
                        error!(
                            "Failed to decrypt `secret_access_key` for bookmark {}: {}",
                            key, err
                        );
                    }
                }
            }
        }
        // Then convert into
        Some(FileTransferParams::from(entry))
    }

    /// Add a new recent to bookmarks
    pub fn add_bookmark<S: AsRef<str>>(
        &mut self,
        name: S,
        params: FileTransferParams,
        save_password: bool,
    ) {
        let name: String = name.as_ref().to_string();
        if name.is_empty() {
            error!("Fatal error; bookmark name is empty");
            panic!("Bookmark name can't be empty");
        }
        // Make bookmark
        info!("Added bookmark {}", name);
        let mut host: Bookmark = self.make_bookmark(params);
        // If not save_password, set secrets to `None`
        if !save_password {
            host.password = None;
            if let Some(s3) = host.s3.as_mut() {
                s3.access_key = None;
                s3.secret_access_key = None;
            }
        }
        self.hosts.bookmarks.insert(name, host);
    }

    /// Delete entry from bookmarks
    pub fn del_bookmark(&mut self, name: &str) {
        let _ = self.hosts.bookmarks.remove(name);
        info!("Removed bookmark {}", name);
    }
    /// Iterate over recents keys
    pub fn iter_recents(&self) -> impl Iterator<Item = &String> + '_ {
        Box::new(self.hosts.recents.keys())
    }

    /// Get recent associated to key
    pub fn get_recent(&self, key: &str) -> Option<FileTransferParams> {
        // NOTE: password is not decrypted; recents will never have password
        info!("Getting bookmark {}", key);
        let entry: Bookmark = self.hosts.recents.get(key).cloned()?;
        Some(FileTransferParams::from(entry))
    }

    /// Add a new recent to bookmarks
    pub fn add_recent(&mut self, params: FileTransferParams) {
        // Make bookmark
        let mut host: Bookmark = self.make_bookmark(params);
        // Null password for recents
        host.password = None;
        if let Some(s3) = host.s3.as_mut() {
            s3.access_key = None;
            s3.secret_access_key = None;
        }
        // Check if duplicated
        for (key, value) in &self.hosts.recents {
            if *value == host {
                debug!("Discarding recent since duplicated ({})", key);
                // Don't save duplicates
                return;
            }
        }
        // If hosts size is bigger than self.recents_size; pop last
        if self.hosts.recents.len() >= self.recents_size {
            // Get keys
            let mut keys: Vec<String> = Vec::with_capacity(self.hosts.recents.len());
            for key in self.hosts.recents.keys() {
                keys.push(key.clone());
            }
            // Sort keys; NOTE: most recent is the last element
            keys.sort();
            // Delete keys starting from the last one
            for key in keys.iter() {
                let _ = self.hosts.recents.remove(key);
                debug!("Removed recent bookmark {}", key);
                // If length is < self.recents_size; break
                if self.hosts.recents.len() < self.recents_size {
                    break;
                }
            }
        }
        let name: String = fmt_time(SystemTime::now(), "ISO%Y%m%dT%H%M%S");
        info!("Saved recent host {}", name);
        self.hosts.recents.insert(name, host);
    }

    /// Delete entry from recents
    pub fn del_recent(&mut self, name: &str) {
        let _ = self.hosts.recents.remove(name);
        info!("Removed recent host {}", name);
    }

    /// Write bookmarks to file
    pub fn write_bookmarks(&self) -> Result<(), SerializerError> {
        // Open file
        debug!("Writing bookmarks");
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.bookmarks_file.as_path())
        {
            Ok(writer) => serialize(&self.hosts, Box::new(writer)),
            Err(err) => {
                error!("Failed to write bookmarks: {}", err);
                Err(SerializerError::new_ex(
                    SerializerErrorKind::Io,
                    err.to_string(),
                ))
            }
        }
    }

    /// Read bookmarks from file
    fn read_bookmarks(&mut self) -> Result<(), SerializerError> {
        // Open bookmarks file for read
        debug!("Reading bookmarks");
        match OpenOptions::new()
            .read(true)
            .open(self.bookmarks_file.as_path())
        {
            Ok(reader) => {
                // Deserialize
                match deserialize(Box::new(reader)) {
                    Ok(hosts) => {
                        self.hosts = hosts;
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => {
                error!("Failed to read bookmarks: {}", err);
                Err(SerializerError::new_ex(
                    SerializerErrorKind::Io,
                    err.to_string(),
                ))
            }
        }
    }

    /// Generate a new AES key
    fn generate_key() -> String {
        // Generate 256 bytes (2048 bits) key
        random_alphanumeric_with_len(256)
    }

    /// Make bookmark from credentials
    fn make_bookmark(&self, params: FileTransferParams) -> Bookmark {
        let mut bookmark: Bookmark = Bookmark::from(params);
        // Encrypt password
        if let Some(pwd) = bookmark.password {
            bookmark.password = Some(self.encrypt_str(pwd.as_str()));
        }
        // Encrypt aws s3 params
        if let Some(s3) = bookmark.s3.as_mut() {
            if let Some(access_key) = s3.access_key.as_mut() {
                *access_key = self.encrypt_str(access_key.as_str());
            }
            if let Some(secret_access_key) = s3.secret_access_key.as_mut() {
                *secret_access_key = self.encrypt_str(secret_access_key.as_str());
            }
        }
        bookmark
    }

    /// Encrypt provided string using AES-128. Encrypted buffer is then converted to BASE64
    fn encrypt_str(&self, txt: &str) -> String {
        crypto::aes128_b64_crypt(self.key.as_str(), txt)
    }

    /// Decrypt provided string using AES-128
    fn decrypt_str(&self, secret: &str) -> Result<String, SerializerError> {
        match crypto::aes128_b64_decrypt(self.key.as_str(), secret) {
            Ok(txt) => Ok(txt),
            Err(err) => Err(SerializerError::new_ex(
                SerializerErrorKind::Syntax,
                err.to_string(),
            )),
        }
    }
}

#[cfg(test)]
#[cfg(not(target_os = "macos"))] // CI/CD blocks
mod tests {

    use std::thread::sleep;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;
    use crate::filetransfer::params::{AwsS3Params, GenericProtocolParams};
    use crate::filetransfer::{FileTransferProtocol, ProtocolParams};

    #[test]

    fn test_system_bookmarks_new() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Verify client
        assert_eq!(client.hosts.bookmarks.len(), 0);
        assert_eq!(client.hosts.recents.len(), 0);
        assert_eq!(client.key.len(), 256);
        assert_eq!(client.bookmarks_file, cfg_path);
        assert_eq!(client.recents_size, 16);
    }

    #[test]

    fn test_system_bookmarks_new_from_existing() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add some bookmarks
        client.add_bookmark(
            "raspberry",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword"),
            ),
            true,
        );
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.31",
            22,
            "pi",
            Some("mypassword"),
        ));
        let recent_key: String = String::from(client.iter_recents().next().unwrap());
        assert!(client.write_bookmarks().is_ok());
        let key: String = client.key.clone();
        // Re-initialize a client
        let client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Verify it loaded parameters correctly
        assert_eq!(client.key, key);
        let bookmark = ftparams_to_tup(client.get_bookmark("raspberry").unwrap());
        assert_eq!(bookmark.0, String::from("192.168.1.31"));
        assert_eq!(bookmark.1, 22);
        assert_eq!(bookmark.2, FileTransferProtocol::Sftp);
        assert_eq!(bookmark.3, String::from("pi"));
        assert_eq!(*bookmark.4.as_ref().unwrap(), String::from("mypassword"));
        let bookmark = ftparams_to_tup(client.get_recent(&recent_key).unwrap());
        assert_eq!(bookmark.0, String::from("192.168.1.31"));
        assert_eq!(bookmark.1, 22);
        assert_eq!(bookmark.2, FileTransferProtocol::Sftp);
        assert_eq!(bookmark.3, String::from("pi"));
        assert_eq!(bookmark.4, None);
    }

    #[test]
    fn should_make_s3_bookmark_with_secrets() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add s3 bookmark
        client.add_bookmark("my-bucket", make_s3_ftparams(), true);
        // Verify bookmark
        let bookmark = client.get_bookmark("my-bucket").unwrap();
        assert_eq!(bookmark.protocol, FileTransferProtocol::AwsS3);
        let params = bookmark.params.s3_params().unwrap();
        assert_eq!(params.access_key.as_deref().unwrap(), "pippo");
        assert_eq!(params.profile.as_deref().unwrap(), "test");
        assert_eq!(params.secret_access_key.as_deref().unwrap(), "pluto");
        assert_eq!(params.bucket_name.as_str(), "omar");
        assert_eq!(params.region.as_deref().unwrap(), "eu-west-1");
    }

    #[test]
    fn should_make_s3_bookmark_without_secrets() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add s3 bookmark
        client.add_bookmark("my-bucket", make_s3_ftparams(), false);
        // Verify bookmark
        let bookmark = client.get_bookmark("my-bucket").unwrap();
        assert_eq!(bookmark.protocol, FileTransferProtocol::AwsS3);
        let params = bookmark.params.s3_params().unwrap();
        assert_eq!(params.profile.as_deref().unwrap(), "test");
        assert_eq!(params.bucket_name.as_str(), "omar");
        assert_eq!(params.region.as_deref().unwrap(), "eu-west-1");
        // secrets
        assert_eq!(params.access_key, None);
        assert_eq!(params.secret_access_key, None);
    }

    #[test]
    fn should_make_s3_recent() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add s3 bookmark
        client.add_recent(make_s3_ftparams());
        // Verify bookmark
        let bookmark = client.iter_recents().next().unwrap();
        let bookmark = client.get_recent(bookmark).unwrap();
        assert_eq!(bookmark.protocol, FileTransferProtocol::AwsS3);
        let params = bookmark.params.s3_params().unwrap();
        assert_eq!(params.profile.as_deref().unwrap(), "test");
        assert_eq!(params.bucket_name.as_str(), "omar");
        assert_eq!(params.region.as_deref().unwrap(), "eu-west-1");
        // secrets
        assert_eq!(params.access_key, None);
        assert_eq!(params.secret_access_key, None);
    }

    #[test]

    fn test_system_bookmarks_manipulate_bookmarks() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_bookmark(
            "raspberry",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword"),
            ),
            true,
        );
        client.add_bookmark(
            "raspberry2",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword2"),
            ),
            true,
        );
        // Iter
        assert_eq!(client.iter_bookmarks().count(), 2);
        // Get bookmark
        let bookmark = ftparams_to_tup(client.get_bookmark(&String::from("raspberry")).unwrap());
        assert_eq!(bookmark.0, String::from("192.168.1.31"));
        assert_eq!(bookmark.1, 22);
        assert_eq!(bookmark.2, FileTransferProtocol::Sftp);
        assert_eq!(bookmark.3, String::from("pi"));
        assert_eq!(*bookmark.4.as_ref().unwrap(), String::from("mypassword"));
        // Write bookmarks
        assert!(client.write_bookmarks().is_ok());
        // Delete bookmark
        client.del_bookmark(&String::from("raspberry"));
        // Get unexisting bookmark
        assert!(client.get_bookmark(&String::from("raspberry")).is_none());
        // Write bookmarks
        assert!(client.write_bookmarks().is_ok());
    }

    #[test]
    #[should_panic]

    fn test_system_bookmarks_bad_bookmark_name() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_bookmark(
            "",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword"),
            ),
            true,
        );
    }

    #[test]
    fn save_bookmark_wno_password() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_bookmark(
            "raspberry",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword"),
            ),
            false,
        );
        let bookmark = ftparams_to_tup(client.get_bookmark(&String::from("raspberry")).unwrap());
        assert_eq!(bookmark.0, String::from("192.168.1.31"));
        assert_eq!(bookmark.1, 22);
        assert_eq!(bookmark.2, FileTransferProtocol::Sftp);
        assert_eq!(bookmark.3, String::from("pi"));
        assert_eq!(bookmark.4, None);
    }

    #[test]

    fn test_system_bookmarks_manipulate_recents() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.31",
            22,
            "pi",
            Some("mypassword"),
        ));
        // Iter
        assert_eq!(client.iter_recents().count(), 1);
        let key: String = String::from(client.iter_recents().next().unwrap());
        // Get bookmark
        let bookmark = ftparams_to_tup(client.get_recent(&key).unwrap());
        assert_eq!(bookmark.0, String::from("192.168.1.31"));
        assert_eq!(bookmark.1, 22);
        assert_eq!(bookmark.2, FileTransferProtocol::Sftp);
        assert_eq!(bookmark.3, String::from("pi"));
        assert_eq!(bookmark.4, None);
        // Write bookmarks
        assert!(client.write_bookmarks().is_ok());
        // Delete bookmark
        client.del_recent(&key);
        // Get unexisting bookmark
        assert!(client.get_bookmark(&key).is_none());
        // Write bookmarks
        assert!(client.write_bookmarks().is_ok());
    }

    #[test]

    fn test_system_bookmarks_dup_recent() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.31",
            22,
            "pi",
            Some("mypassword"),
        ));
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.31",
            22,
            "pi",
            Some("mypassword"),
        ));
        // There should be only one recent
        assert_eq!(client.iter_recents().count(), 1);
    }

    #[test]

    fn test_system_bookmarks_recents_more_than_limit() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 2, true).unwrap();
        // Add recent, wait 1 second for each one (cause the name depends on time)
        // 1
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.1",
            22,
            "pi",
            Some("mypassword"),
        ));
        sleep(Duration::from_secs(1));
        // 2
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.2",
            22,
            "pi",
            Some("mypassword"),
        ));
        sleep(Duration::from_secs(1));
        // 3
        client.add_recent(make_generic_ftparams(
            FileTransferProtocol::Sftp,
            "192.168.1.3",
            22,
            "pi",
            Some("mypassword"),
        ));
        // Limit is 2
        assert_eq!(client.iter_recents().count(), 2);
        // Check that 192.168.1.1 has been removed
        let key: String = client.iter_recents().next().unwrap().to_string();
        assert!(matches!(
            client
                .hosts
                .recents
                .get(&key)
                .unwrap()
                .address
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .as_str(),
            "192.168.1.2" | "192.168.1.3"
        ));
        let key: String = client.iter_recents().nth(1).unwrap().to_string();
        assert!(matches!(
            client
                .hosts
                .recents
                .get(&key)
                .unwrap()
                .address
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .as_str(),
            "192.168.1.2" | "192.168.1.3"
        ));
    }

    #[test]
    #[should_panic]
    fn test_system_bookmarks_add_bookmark_empty() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        // Add bookmark
        client.add_bookmark(
            "",
            make_generic_ftparams(
                FileTransferProtocol::Sftp,
                "192.168.1.31",
                22,
                "pi",
                Some("mypassword"),
            ),
            true,
        );
    }

    #[test]
    fn test_system_bookmarks_decrypt_str() {
        let tmp_dir: tempfile::TempDir = TempDir::new().ok().unwrap();
        let (cfg_path, key_path): (PathBuf, PathBuf) = get_paths(tmp_dir.path());
        // Initialize a new bookmarks client
        let mut client: BookmarksClient =
            BookmarksClient::new(cfg_path.as_path(), key_path.as_path(), 16, true).unwrap();
        client.key = "MYSUPERSECRETKEY".to_string();
        assert_eq!(
            client.decrypt_str("z4Z6LpcpYqBW4+bkIok+5A==").ok().unwrap(),
            "Hello world!"
        );
        assert!(client.decrypt_str("bidoof").is_err());
    }

    /// Get paths for configuration and key for bookmarks
    fn get_paths(dir: &Path) -> (PathBuf, PathBuf) {
        let k: PathBuf = PathBuf::from(dir);
        let mut c: PathBuf = k.clone();
        c.push("bookmarks.toml");
        (c, k)
    }

    fn make_generic_ftparams(
        protocol: FileTransferProtocol,
        address: &str,
        port: u16,
        username: &str,
        password: Option<&str>,
    ) -> FileTransferParams {
        let params = ProtocolParams::Generic(
            GenericProtocolParams::default()
                .address(address)
                .port(port)
                .username(Some(username))
                .password(password),
        );
        FileTransferParams::new(protocol, params)
    }

    fn make_s3_ftparams() -> FileTransferParams {
        FileTransferParams::new(
            FileTransferProtocol::AwsS3,
            ProtocolParams::AwsS3(
                AwsS3Params::new("omar", Some("eu-west-1"), Some("test"))
                    .endpoint(Some("http://localhost:9000"))
                    .new_path_style(false)
                    .access_key(Some("pippo"))
                    .secret_access_key(Some("pluto"))
                    .security_token(Some("omar"))
                    .session_token(Some("gerry-scotti")),
            ),
        )
    }

    fn ftparams_to_tup(
        params: FileTransferParams,
    ) -> (String, u16, FileTransferProtocol, String, Option<String>) {
        let protocol = params.protocol;
        let p = params.params.generic_params().unwrap();
        (
            p.address.to_string(),
            p.port,
            protocol,
            p.username.as_ref().cloned().unwrap_or_default(),
            p.password.as_ref().cloned(),
        )
    }
}
