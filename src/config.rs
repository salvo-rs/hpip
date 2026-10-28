use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use cidr::IpCidr;

use crate::options::{LogLevel, Options, WebDavLevel};

pub type CacheT<Cnt> = HashMap<(blake3::Hash, EncodingType), (Cnt, AtomicU64)>;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum EncodingType {
    Gzip,
    Deflate,
    Brotli,
}

pub struct AppConfig {
    pub hosted_directory: (String, PathBuf),
    pub follow_symlinks: bool,
    pub sandbox_symlinks: bool,
    pub generate_listings: bool,
    pub check_indices: bool,
    pub strip_extensions: bool,
    pub try_404: Option<PathBuf>,
    /// (at all, log_time, log_colour)
    pub log: (bool, bool, bool),
    pub webdav: WebDavLevel,
    pub archives: bool,
    pub global_auth_data: Option<(String, Option<String>)>,
    pub path_auth_data: BTreeMap<String, Option<(String, Option<String>)>>,
    pub writes_temp_dir: Option<(String, PathBuf)>,
    pub encoded_temp_dir: Option<(String, PathBuf)>,
    #[allow(dead_code)]
    pub proxies: BTreeMap<IpCidr, String>,
    #[allow(dead_code)]
    pub proxy_redirs: BTreeMap<IpCidr, String>,
    pub mime_type_overrides: BTreeMap<OsString, String>,
    pub additional_headers: Vec<(String, Vec<u8>)>,
    #[allow(dead_code)]
    pub request_bandwidth: Option<NonZeroU64>,

    pub cache_gen: RwLock<CacheT<Vec<u8>>>,
    pub cache_fs_files: RwLock<HashMap<String, blake3::Hash>>,
    pub cache_fs: RwLock<CacheT<(PathBuf, bool, u64)>>,
    pub cache_gen_size: AtomicU64,
    pub cache_fs_size: AtomicU64,
    pub encoded_filesystem_limit: u64,
    pub encoded_generated_limit: u64,

    pub encoded_prune: Option<u64>,
    pub prune_interval: u64,
    pub last_prune: AtomicU64,

    pub allowed_methods: Vec<String>,
}

impl AppConfig {
    /// Returns true if a symlink should be denied access based on follow/sandbox settings.
    pub fn is_symlink_denied(&self, symlink: bool, path: &Path) -> bool {
        symlink
            && (!self.follow_symlinks
                || (self.sandbox_symlinks
                    && !crate::util::path::is_descendant_of(path, &self.hosted_directory.1)))
    }

    /// Like `is_symlink_denied` but uses nonexistent-descendant check (for PUT/create).
    pub fn is_symlink_denied_nonexistent(&self, symlink: bool, path: &Path) -> bool {
        symlink
            && (!self.follow_symlinks
                || (self.sandbox_symlinks
                    && !crate::util::path::is_nonexistent_descendant_of(
                        path,
                        &self.hosted_directory.1,
                    )))
    }

    pub fn new(opts: &Options) -> AppConfig {
        let mut path_auth_data = BTreeMap::new();
        let mut global_auth_data = None;

        for (path, creds) in &opts.path_auth_data {
            let creds = creds.as_ref().map(|auth| {
                let mut itr = auth.split_terminator(':');
                (
                    itr.next().unwrap().to_string(),
                    itr.next().map(str::to_string),
                )
            });

            if path.is_empty() {
                global_auth_data = creds;
            } else {
                path_auth_data.insert(path.to_string(), creds);
            }
        }

        let mut allowed_methods = vec!["OPTIONS", "GET", "HEAD", "TRACE"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();

        if opts.allow_writes {
            allowed_methods.push("PUT".into());
            allowed_methods.push("DELETE".into());
        }

        if opts.webdav == WebDavLevel::All {
            allowed_methods.push("PROPFIND".into());
            allowed_methods.push("PROPPATCH".into());
            allowed_methods.push("MKCOL".into());
            allowed_methods.push("COPY".into());
            allowed_methods.push("MOVE".into());
        } else if opts.webdav == WebDavLevel::MkColMoveOnly {
            allowed_methods.push("MKCOL".into());
            allowed_methods.push("MOVE".into());
        }

        // If WebDAV but not writes, only PROPFIND is read-only
        if opts.webdav == WebDavLevel::All && !opts.allow_writes {
            // Remove write-only WebDAV methods; PROPFIND is read-only
            allowed_methods.retain(|m| {
                !matches!(
                    m.as_str(),
                    "PROPPATCH" | "MKCOL" | "COPY" | "MOVE" | "PUT" | "DELETE"
                )
            });
            allowed_methods.push("PROPFIND".into());
        }

        AppConfig {
            hosted_directory: opts.hosted_directory.clone(),
            follow_symlinks: opts.follow_symlinks,
            sandbox_symlinks: opts.sandbox_symlinks,
            generate_listings: opts.generate_listings,
            check_indices: opts.check_indices,
            strip_extensions: opts.strip_extensions,
            try_404: opts.try_404.clone(),
            log: (
                opts.loglevel < LogLevel::NoServeStatus,
                opts.log_time,
                opts.log_colour,
            ),
            webdav: opts.webdav,
            archives: opts.archives,
            global_auth_data,
            path_auth_data,
            writes_temp_dir: AppConfig::temp_subdir(
                &opts.temp_directory,
                opts.allow_writes,
                "writes",
            ),
            encoded_temp_dir: AppConfig::temp_subdir(
                &opts.temp_directory,
                opts.encode_fs,
                "encoded",
            ),
            cache_gen: Default::default(),
            cache_fs: Default::default(),
            cache_fs_files: Default::default(),
            cache_gen_size: Default::default(),
            cache_fs_size: Default::default(),
            encoded_filesystem_limit: opts.encoded_filesystem_limit.unwrap_or(256 * 1024 * 1024),
            encoded_generated_limit: opts.encoded_generated_limit.unwrap_or(16 * 1024 * 1024),
            encoded_prune: opts.encoded_prune,
            prune_interval: (opts.encoded_prune.unwrap_or(0) / 6).max(10),
            last_prune: AtomicU64::new(0),
            proxies: opts.proxies.clone(),
            proxy_redirs: opts.proxy_redirs.clone(),
            mime_type_overrides: opts.mime_type_overrides.clone(),
            additional_headers: opts.additional_headers.clone(),
            request_bandwidth: opts.request_bandwidth,
            allowed_methods,
        }
    }

    /// Admit one completed cache file while holding the same lock as eviction.
    pub fn cache_file(&self, key: (blake3::Hash, EncodingType), path: PathBuf, size: u64) -> bool {
        if size > self.encoded_filesystem_limit {
            return false;
        }
        let mut indices = self.cache_fs_files.write().unwrap();
        let mut cache = self.cache_fs.write().unwrap();
        if let Some(((old_path, ..), _)) = cache.remove(&key)
            && old_path != path
        {
            let _ = fs::remove_file(old_path);
        }
        let mut used = cache.values().map(|((_, _, size), _)| size).sum::<u64>();
        while used > self.encoded_filesystem_limit - size {
            let Some(key) = cache
                .iter()
                .min_by_key(|(_, (_, time))| time.load(AtomicOrdering::Relaxed))
                .map(|(key, _)| *key)
            else {
                return false;
            };
            let ((old_path, present, old_size), _) = cache.remove(&key).unwrap();
            if present {
                let _ = fs::remove_file(old_path);
            }
            used -= old_size;
            indices.retain(|_, hash| *hash != key.0);
        }
        cache.insert(
            key,
            (
                (path, true, size),
                AtomicU64::new(crate::util::precise_time_ns()),
            ),
        );
        self.cache_fs_size
            .store(used + size, AtomicOrdering::Relaxed);
        true
    }

    pub fn cache_generated(&self, key: (blake3::Hash, EncodingType), data: &[u8]) {
        let size = data.len() as u64;
        if size > self.encoded_generated_limit {
            return;
        }
        let mut cache = self.cache_gen.write().unwrap();
        cache.remove(&key);
        let mut used = cache
            .values()
            .map(|(data, _)| data.len() as u64)
            .sum::<u64>();
        while used > self.encoded_generated_limit - size {
            let Some(key) = cache
                .iter()
                .min_by_key(|(_, (_, time))| time.load(AtomicOrdering::Relaxed))
                .map(|(key, _)| *key)
            else {
                return;
            };
            used -= cache.remove(&key).unwrap().0.len() as u64;
        }
        cache.insert(
            key,
            (
                data.to_vec(),
                AtomicU64::new(crate::util::precise_time_ns()),
            ),
        );
        self.cache_gen_size
            .store(used + size, AtomicOrdering::Relaxed);
    }

    pub fn create_temp_dir(&self, td: &Option<(String, PathBuf)>) {
        if let Some((temp_name, temp_dir)) = td
            && !temp_dir.exists()
            && fs::create_dir_all(temp_dir).is_ok()
        {
            log_msg(self.log, &format!("Created temp dir {}", temp_name));
        }
    }

    pub fn clean_temp_dirs(&self, temp_directory: &(String, PathBuf), generate_tls: bool) {
        let tls = AppConfig::temp_subdir(temp_directory, generate_tls, "tls");
        for (temp_name, temp_dir) in [
            self.writes_temp_dir.as_ref(),
            self.encoded_temp_dir.as_ref(),
            tls.as_ref(),
        ]
        .iter()
        .flatten()
        {
            if fs::remove_dir_all(temp_dir).is_ok() {
                log_msg(self.log, &format!("Deleted temp dir {}", temp_name));
            }
        }
        if fs::remove_dir(&temp_directory.1).is_ok() {
            log_msg(self.log, &format!("Deleted temp dir {}", temp_directory.0));
        }
    }

    pub fn prune(&self) {
        use std::collections::HashSet;

        use crate::util::precise_time_ns;

        let mut start = 0u64;
        let mut freed_fs = 0u64;
        let mut freed_gen = 0u64;

        if let Some(limit) = self.encoded_filesystem_limit.checked_sub(0)
            && limit < u64::MAX
            && self.cache_fs_size.load(AtomicOrdering::Relaxed) > limit
        {
            start = precise_time_ns();

            let mut cache_files = self
                .cache_fs_files
                .write()
                .expect("Filesystem files cache write lock poisoned");
            let mut removed_file_hashes = HashSet::new();
            let mut cache = self
                .cache_fs
                .write()
                .expect("Filesystem cache write lock poisoned");
            let size = self.cache_fs_size.load(AtomicOrdering::Relaxed);
            while size - freed_fs > limit {
                let key = match cache
                    .iter()
                    .min_by_key(|i| (i.1).1.load(AtomicOrdering::Relaxed))
                {
                    Some((key, ((path, ..), _))) => match fs::remove_file(path) {
                        Ok(()) => *key,
                        Err(_) => break,
                    },
                    None => break,
                };
                let ((_, _, sz), _) = cache.remove(&key).unwrap();
                freed_fs += sz;
                removed_file_hashes.insert(key.0);
            }
            self.cache_fs_size
                .fetch_sub(freed_fs, AtomicOrdering::Relaxed);
            cache_files.retain(|_, v| !removed_file_hashes.contains(v));
        }

        if let Some(limit) = self.encoded_generated_limit.checked_sub(0)
            && limit < u64::MAX
            && self.cache_gen_size.load(AtomicOrdering::Relaxed) > limit
        {
            if start == 0 {
                start = precise_time_ns();
            }

            let mut cache = self
                .cache_gen
                .write()
                .expect("Generated file cache write lock poisoned");
            let size = self.cache_gen_size.load(AtomicOrdering::Relaxed);
            while size - freed_gen > limit {
                let key = match cache
                    .iter()
                    .min_by_key(|i| (i.1).1.load(AtomicOrdering::Relaxed))
                {
                    Some((key, _)) => *key,
                    None => break,
                };
                let (data, _) = cache.remove(&key).unwrap();
                freed_gen += data.len() as u64;
            }
            self.cache_gen_size
                .fetch_sub(freed_gen, AtomicOrdering::Relaxed);
        }

        if let Some(limit) = self.encoded_prune {
            if start == 0 {
                start = precise_time_ns();
            }

            let last = self.last_prune.swap(start, AtomicOrdering::Relaxed);
            if last < start && (start - last) / 1_000_000_000 >= self.prune_interval {
                {
                    let mut cache_files = self
                        .cache_fs_files
                        .write()
                        .expect("Filesystem files cache write lock poisoned");
                    let mut removed_file_hashes = HashSet::new();
                    let mut cache = self
                        .cache_fs
                        .write()
                        .expect("Filesystem cache write lock poisoned");
                    cache.retain(|(hash, _), ((path, _, sz), atime)| {
                        let atime = atime.load(AtomicOrdering::Relaxed);
                        if atime > start || (start - atime) / 1_000_000_000 <= limit {
                            return true;
                        }

                        if fs::remove_file(path).is_err() {
                            return true;
                        }
                        freed_fs += *sz;
                        self.cache_fs_size.fetch_sub(*sz, AtomicOrdering::Relaxed);
                        removed_file_hashes.insert(*hash);
                        false
                    });
                    cache_files.retain(|_, v| !removed_file_hashes.contains(v));
                }
                {
                    let mut cache = self
                        .cache_gen
                        .write()
                        .expect("Generated file cache write lock poisoned");
                    cache.retain(|_, (data, atime)| {
                        let atime = atime.load(AtomicOrdering::Relaxed);
                        if atime > start || (start - atime) / 1_000_000_000 <= limit {
                            return true;
                        }

                        freed_gen += data.len() as u64;
                        self.cache_gen_size
                            .fetch_sub(data.len() as u64, AtomicOrdering::Relaxed);
                        false
                    });
                }
            }
        }

        if freed_fs != 0 || freed_gen != 0 {
            use crate::util::HumanReadableSize;
            let end = precise_time_ns();
            log_msg(
                self.log,
                &format!(
                    "Pruned {} + {} in {}ns; used: {} + {}",
                    HumanReadableSize(freed_fs),
                    HumanReadableSize(freed_gen),
                    end - start,
                    HumanReadableSize(self.cache_fs_size.load(AtomicOrdering::Relaxed)),
                    HumanReadableSize(self.cache_gen_size.load(AtomicOrdering::Relaxed))
                ),
            );
        }
    }

    fn temp_subdir(
        (temp_name, temp_dir): &(String, PathBuf),
        flag: bool,
        name: &str,
    ) -> Option<(String, PathBuf)> {
        if flag {
            Some((
                format!(
                    "{}{}{}",
                    temp_name,
                    if temp_name.ends_with('/') || temp_name.ends_with('\\') {
                        ""
                    } else {
                        "/"
                    },
                    name
                ),
                temp_dir.join(name),
            ))
        } else {
            None
        }
    }
}

pub fn log_msg(log: (bool, bool, bool), msg: &str) {
    if log.0 {
        if log.2 {
            // coloured
            use colored::Colorize;
            if log.1 {
                print!(
                    "{} ",
                    format!("[{}]", chrono::Local::now().format("%F %T")).cyan()
                );
            }
            println!("{}", msg);
        } else {
            if log.1 {
                print!("[{}] ", chrono::Local::now().format("%F %T"));
            }
            println!("{}", msg);
        }
    }
}

#[cfg(test)]
mod cache_budget_tests {
    use super::*;

    #[test]
    fn file_admission_enforces_total_without_a_prune_timer() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &["--encoded-filesystem", "150"]);
        for number in 0..4u8 {
            let path = dir.path().join(format!("cache-{number}"));
            fs::write(&path, [number; 65]).unwrap();
            assert!(config.cache_file((blake3::hash(&[number]), EncodingType::Gzip), path, 65));
            assert!(config.cache_fs_size.load(AtomicOrdering::Relaxed) <= 150);
            let actual = config
                .cache_fs
                .read()
                .unwrap()
                .values()
                .map(|((path, ..), _)| fs::metadata(path).unwrap().len())
                .sum::<u64>();
            assert_eq!(actual, config.cache_fs_size.load(AtomicOrdering::Relaxed));
        }
        assert_eq!(config.cache_fs.read().unwrap().len(), 2);
    }

    #[test]
    fn generated_admission_counts_replacements_once_and_evicts() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &["--encoded-generated", "150"]);
        let key = (blake3::hash(b"same"), EncodingType::Gzip);
        config.cache_generated(key, &[1; 60]);
        config.cache_generated(key, &[2; 65]);
        assert_eq!(config.cache_gen_size.load(AtomicOrdering::Relaxed), 65);
        for number in 0..4u8 {
            config.cache_generated((blake3::hash(&[number]), EncodingType::Gzip), &[number; 65]);
            assert!(config.cache_gen_size.load(AtomicOrdering::Relaxed) <= 150);
        }
        assert_eq!(config.cache_gen.read().unwrap().len(), 2);
    }
}
