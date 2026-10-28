use std::collections::BTreeSet;
use std::collections::btree_map::{BTreeMap, Entry as BTreeMapEntry};
use std::env;
use std::ffi::OsString;
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;

use cidr::IpCidr;
use clap::Parser;

use crate::error::Error;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    All,
    NoServeStatus,
    NoStartup,
    NoAuth,
}

impl From<u8> for LogLevel {
    fn from(raw: u8) -> LogLevel {
        match raw {
            0 => LogLevel::All,
            1 => LogLevel::NoServeStatus,
            2 => LogLevel::NoStartup,
            _ => LogLevel::NoAuth,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum WebDavLevel {
    No,
    MkColMoveOnly,
    All,
}

/// An asynchronous HTTP file server with WebDAV, TLS, and authentication
#[derive(Parser, Debug)]
#[command(name = "hpip", version, about)]
struct Cli {
    /// Directory to host. Default: current working directory
    #[arg(default_value = ".")]
    dir: String,

    /// Port to use. Default: first free port from 8000 up
    #[arg(short, long)]
    port: Option<u16>,

    /// Address to bind to. Default: 0.0.0.0
    #[arg(short = 'a', long = "address")]
    address: Option<String>,

    /// Temporary directory. Default: $TEMP
    #[arg(short = 't', long = "temp-dir")]
    temp_dir: Option<String>,

    /// Return this file instead of a 404 for a GET. Default: generated response
    #[arg(long = "404")]
    fallback_404: Option<String>,

    /// Don't follow symlinks. Default: false
    #[arg(short = 's', long = "no-follow-symlinks")]
    no_follow_symlinks: bool,

    /// Restrict/sandbox where symlinks lead to only the direct descendants of the hosted directory
    #[arg(short = 'r', long = "sandbox-symlinks")]
    sandbox_symlinks: bool,

    /// Allow for write operations. Default: false
    #[arg(short = 'w', long = "allow-write")]
    allow_write: bool,

    /// Maximum upload size in bytes (suffixes K, M, G accepted). Default: 1G
    #[arg(long = "max-upload-size", default_value = "1G")]
    max_upload_size: String,

    /// Never generate dir listings. Default: false
    #[arg(short = 'l', long = "no-listings")]
    no_listings: bool,

    /// Do not automatically use index files. Default: false
    #[arg(short = 'i', long = "no-indices")]
    no_indices: bool,

    /// Do not encode filesystem files. Default: false
    #[arg(short = 'e', long = "no-encode")]
    no_encode: bool,

    /// Consume at most FS_LIMIT space for encoded filesystem files
    #[arg(long = "encoded-filesystem")]
    encoded_filesystem: Option<String>,

    /// Consume at most GEN_LIMIT memory for encoded generated responses
    #[arg(long = "encoded-generated")]
    encoded_generated: Option<String>,

    /// Prune cached encoded data older than MAX_AGE
    #[arg(long = "encoded-prune")]
    encoded_prune: Option<String>,

    /// Allow stripping index extensions from served paths. Default: false
    #[arg(short = 'x', long = "strip-extensions")]
    strip_extensions: bool,

    /// Suppress increasing amounts of output
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,

    /// Don't prefix logs with the timestamp
    #[arg(short = 'Q', long = "quiet-time")]
    quiet_time: bool,

    /// Don't colourise the log output
    #[arg(short = 'c', long = "no-colour")]
    no_colour: bool,

    /// Handle WebDAV requests (MKCOL, MOVE, PROPFIND, COPY, PROPPATCH). Default: false
    #[arg(short = 'd', long = "webdav")]
    webdav: bool,

    /// Allow WebDAV MKCOL and MOVE only. Default: false
    #[arg(short = 'D', long = "convenient-webdav")]
    convenient_webdav: bool,

    /// Allow requesting tar and ZIP archives. Default: false
    #[arg(short = 'A', long = "archives")]
    archives: bool,

    /// Data for HTTPS, identity file. Password in HTTP_SSL_PASS env var, otherwise empty
    #[arg(long = "ssl")]
    ssl: Option<String>,

    /// Generate a one-off TLS certificate
    #[arg(long = "gen-ssl", conflicts_with = "ssl")]
    gen_ssl: bool,

    /// Data for global authentication
    #[arg(long = "auth")]
    auth: Option<String>,

    /// Generate a one-off username:password set for global authentication
    #[arg(long = "gen-auth", conflicts_with = "auth")]
    gen_auth: bool,

    /// Data for authentication under PATH
    #[arg(long = "path-auth", num_args = 1)]
    path_auth: Vec<String>,

    /// Generate a one-off username:password set for authentication under PATH
    #[arg(long = "gen-path-auth", num_args = 1)]
    gen_path_auth: Vec<String>,

    /// Reserved proxy forwarding setting; currently rejected as unsupported
    #[arg(long = "proxy", num_args = 1)]
    proxy: Vec<String>,

    /// Reserved proxy redirect setting; currently rejected as unsupported
    #[arg(long = "proxy-redir", num_args = 1)]
    proxy_redir: Vec<String>,

    /// Always return MIME-TYPE for files with EXTENSION
    #[arg(short = 'm', long = "mime-type", num_args = 1)]
    mime_type: Vec<String>,

    /// Bandwidth setting: only 0 (unlimited) is supported; nonzero values are rejected
    #[arg(long = "request-bandwidth")]
    request_bandwidth: Option<String>,

    /// Headers to add to every response
    #[arg(short = 'H', long = "header", num_args = 1)]
    header: Vec<String>,
}

/// All configurable values for the application.
#[derive(Debug, Clone)]
pub struct Options {
    pub hosted_directory: (String, PathBuf),
    pub port: Option<u16>,
    pub bind_address: IpAddr,
    pub follow_symlinks: bool,
    pub sandbox_symlinks: bool,
    pub temp_directory: (String, PathBuf),
    pub generate_listings: bool,
    pub check_indices: bool,
    pub strip_extensions: bool,
    pub try_404: Option<PathBuf>,
    pub allow_writes: bool,
    pub upload_limit: u64,
    pub encode_fs: bool,
    pub encoded_filesystem_limit: Option<u64>,
    pub encoded_generated_limit: Option<u64>,
    pub encoded_prune: Option<u64>,
    pub loglevel: LogLevel,
    pub log_time: bool,
    pub log_colour: bool,
    pub webdav: WebDavLevel,
    pub archives: bool,
    pub tls_data: Option<((String, PathBuf), String)>,
    pub generate_tls: bool,
    pub path_auth_data: BTreeMap<String, Option<String>>,
    pub generate_path_auth: BTreeSet<String>,
    pub proxies: BTreeMap<IpCidr, String>,
    pub proxy_redirs: BTreeMap<IpCidr, String>,
    pub mime_type_overrides: BTreeMap<OsString, String>,
    pub request_bandwidth: Option<NonZeroU64>,
    pub additional_headers: Vec<(String, Vec<u8>)>,
}

impl Options {
    pub fn parse() -> Result<Options, Error> {
        Self::from_cli(Cli::parse())
    }

    #[cfg(test)]
    pub(crate) fn parse_test(root: &std::path::Path, flags: &[&str]) -> Result<Options, Error> {
        let mut args = vec![OsString::from("hpip"), root.as_os_str().to_owned()];
        args.extend(flags.iter().map(OsString::from));
        Self::from_cli(Cli::try_parse_from(args).map_err(|e| Error(e.to_string()))?)
    }

    fn from_cli(cli: Cli) -> Result<Options, Error> {
        if !cli.proxy.is_empty() || !cli.proxy_redir.is_empty() {
            return Err(Error(
                "Proxy header options are not implemented; remove --proxy and --proxy-redir".into(),
            ));
        }
        if cli
            .request_bandwidth
            .as_deref()
            .map(Self::bandwidth_parse)
            .transpose()?
            .flatten()
            .is_some()
        {
            return Err(Error("Per-request bandwidth limiting is not implemented; remove --request-bandwidth or set it to 0".into()));
        }
        let dir = &cli.dir;
        let dir_pb = std::fs::canonicalize(dir)
            .map_err(|_| Error(format!("Directory to host \"{}\" not found", dir)))?;
        if !dir_pb.is_dir() {
            return Err(Error(format!(
                "Directory to host \"{}\" not actually a directory",
                dir
            )));
        }

        let follow_symlinks = !cli.no_follow_symlinks;

        let bind_address = cli
            .address
            .as_deref()
            .map(IpAddr::from_str)
            .transpose()
            .map_err(|_| Error("Invalid bind address".into()))?
            .unwrap_or_else(|| "0.0.0.0".parse().unwrap());

        let mut path_auth_data = BTreeMap::new();
        if let Some(ref root_auth) = cli.auth {
            Options::validate_credentials(root_auth)?;
            path_auth_data.insert(
                String::new(),
                Some(Options::normalise_credentials(root_auth)),
            );
        }

        for pa in &cli.path_auth {
            let (path, auth) = Options::decode_path_credentials(pa)?;
            match path_auth_data.entry(path) {
                BTreeMapEntry::Occupied(oe) => {
                    return Err(Error(format!(
                        "Credentials for path \"/{}\" already present",
                        oe.key()
                    )));
                }
                BTreeMapEntry::Vacant(ve) => {
                    ve.insert(auth.map(|s| Options::normalise_credentials(&s)));
                }
            }
        }

        let mut generate_path_auth = BTreeSet::new();
        if cli.gen_auth {
            generate_path_auth.insert(String::new());
        }

        for gpa in &cli.gen_path_auth {
            let path = Options::normalise_path(gpa);
            if path_auth_data.contains_key(&path) {
                return Err(Error(format!(
                    "Credentials for path \"/{}\" already present",
                    path
                )));
            }
            if !generate_path_auth.insert(path.clone()) {
                return Err(Error(format!(
                    "Credentials for path \"/{}\" already present",
                    path
                )));
            }
        }

        let proxies: BTreeMap<IpCidr, String> = cli
            .proxy
            .iter()
            .map(|s| Options::proxy_parse(s))
            .collect::<Result<_, _>>()?;

        let proxy_redirs: BTreeMap<IpCidr, String> = cli
            .proxy_redir
            .iter()
            .map(|s| Options::proxy_parse(s))
            .collect::<Result<_, _>>()?;

        let mime_type_overrides: BTreeMap<OsString, String> = cli
            .mime_type
            .iter()
            .map(|s| Options::mime_type_override_parse(s))
            .collect::<Result<_, _>>()?;

        let additional_headers: Vec<(String, Vec<u8>)> = cli
            .header
            .iter()
            .map(|s| Options::header_parse(s))
            .collect::<Result<_, _>>()?;

        let webdav = std::cmp::max(
            if cli.webdav {
                WebDavLevel::All
            } else {
                WebDavLevel::No
            },
            if cli.convenient_webdav {
                WebDavLevel::MkColMoveOnly
            } else {
                WebDavLevel::No
            },
        );

        Ok(Options {
            hosted_directory: (dir.to_string(), dir_pb.clone()),
            port: cli.port,
            bind_address,
            follow_symlinks,
            sandbox_symlinks: follow_symlinks && cli.sandbox_symlinks,
            temp_directory: {
                let (temp_s, temp_pb) = if let Some(ref tmpdir) = cli.temp_dir {
                    (
                        tmpdir.to_string(),
                        std::fs::canonicalize(tmpdir).map_err(|_| {
                            Error(format!("Temporary directory \"{}\" not found", tmpdir))
                        })?,
                    )
                } else {
                    ("$TEMP".to_string(), env::temp_dir())
                };
                static NEXT_INSTANCE: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| Error(format!("Creating temporary namespace: {e}")))?
                    .as_nanos();
                let suffix = format!(
                    ".hpip-internal-{}-{}-{}",
                    std::process::id(),
                    timestamp,
                    NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                (
                    format!(
                        "{}{}{}",
                        temp_s,
                        if temp_s.ends_with('/') || temp_s.ends_with('\\') {
                            ""
                        } else {
                            "/"
                        },
                        suffix
                    ),
                    temp_pb.join(&suffix),
                )
            },
            generate_listings: !cli.no_listings,
            check_indices: !cli.no_indices,
            strip_extensions: cli.strip_extensions,
            try_404: cli.fallback_404.map(PathBuf::from),
            allow_writes: cli.allow_write,
            upload_limit: Self::size_parse(&cli.max_upload_size)?,
            encode_fs: !cli.no_encode,
            encoded_filesystem_limit: cli
                .encoded_filesystem
                .as_deref()
                .map(Options::size_parse)
                .transpose()?,
            encoded_generated_limit: cli
                .encoded_generated
                .as_deref()
                .map(Options::size_parse)
                .transpose()?,
            encoded_prune: cli
                .encoded_prune
                .as_deref()
                .map(Options::age_parse)
                .transpose()?,
            loglevel: LogLevel::from(cli.quiet),
            log_time: !cli.quiet_time,
            log_colour: !cli.no_colour,
            webdav,
            archives: cli.archives,
            tls_data: cli
                .ssl
                .map(|id| {
                    let id_pb = std::fs::canonicalize(&id)
                        .map_err(|e| Error(format!("TLS identity file unavailable: {e}")))?;
                    Ok::<_, Error>(((id, id_pb), env::var("HTTP_SSL_PASS").unwrap_or_default()))
                })
                .transpose()?,
            generate_tls: cli.gen_ssl,
            path_auth_data,
            generate_path_auth,
            proxies,
            proxy_redirs,
            mime_type_overrides,
            request_bandwidth: cli
                .request_bandwidth
                .as_deref()
                .map(Options::bandwidth_parse)
                .transpose()?
                .flatten(),
            additional_headers,
        })
    }

    fn validate_credentials(s: &str) -> Result<(), Error> {
        if match s.split_once(':') {
            Some((u, p)) => !u.is_empty() && !p.contains(':'),
            None => !s.is_empty(),
        } {
            Ok(())
        } else {
            Err(Error(format!(
                "Global authentication credentials \"{}\" need be in format \"username[:password]\"",
                s
            )))
        }
    }

    fn decode_path_credentials(s: &str) -> Result<(String, Option<String>), Error> {
        let (path, creds) = s.split_once('=').ok_or_else(|| {
            Error(format!(
                "Per-path authentication credentials \"{}\" need be in format \"path=[username[:password]]\"",
                s
            ))
        })?;

        let path = Options::normalise_path(path);
        if creds.is_empty() {
            Ok((path, None))
        } else {
            if let Some((u, p)) = creds.split_once(':')
                && (u.is_empty() || p.contains(':'))
            {
                return Err(Error(format!(
                    "Per-path authentication credentials \"{}\" need be in format \"path=[username[:password]]\"",
                    s
                )));
            }
            Ok((path, Some(creds.to_string())))
        }
    }

    fn normalise_path(path: &str) -> String {
        let mut frags = vec![];
        for fragment in path.split(['/', '\\']) {
            match fragment {
                "" | "." => {}
                ".." => {
                    frags.pop();
                }
                _ => frags.push(fragment),
            }
        }

        let mut ret =
            String::with_capacity(frags.iter().map(|s| s.len()).sum::<usize>() + frags.len());
        for frag in frags {
            ret.push_str(frag);
            ret.push('/');
        }
        ret.pop();
        ret
    }

    fn normalise_credentials(creds: &str) -> String {
        if creds.ends_with(':') {
            creds[0..creds.len() - 1].to_string()
        } else {
            creds.to_string()
        }
    }

    pub fn size_parse(s: &str) -> Result<u64, Error> {
        let mut s = s;
        if matches!(s.as_bytes().last(), Some(b'b' | b'B')) {
            s = &s[..s.len() - 1];
        }
        let mul: u64 = match s.as_bytes().last() {
            Some(b'k' | b'K') => 1024,
            Some(b'm' | b'M') => 1024 * 1024,
            Some(b'g' | b'G') => 1024 * 1024 * 1024,
            Some(b't' | b'T') => 1024 * 1024 * 1024 * 1024,
            Some(b'p' | b'P') => 1024 * 1024 * 1024 * 1024 * 1024,
            _ => 1,
        };
        if mul != 1 {
            s = &s[..s.len() - 1];
        }
        s.parse::<u64>()
            .map(|size| size * mul)
            .map_err(|e| Error(format!("{} not a valid size: {}", s, e)))
    }

    pub fn age_parse(s: &str) -> Result<u64, Error> {
        let mut s = s;
        let (mul, trim) = match s.as_bytes().last() {
            Some(b's') => (1, true),
            Some(b'm') => (60, true),
            Some(b'h') => (60 * 60, true),
            Some(b'd') => (60 * 60 * 24, true),
            _ => (1, false),
        };
        if trim {
            s = &s[..s.len() - 1];
        }
        s.parse::<u64>()
            .map(|age| age * mul)
            .map_err(|e| Error(format!("{} not a valid age: {}", s, e)))
    }

    fn proxy_parse(s: &str) -> Result<(IpCidr, String), Error> {
        match s.find(':') {
            None => Err(Error(format!("{} not in HEADER-NAME:CIDR format", s))),
            Some(0) => Err(Error(format!("{} sets invalid zero-length header", s))),
            Some(col_idx) => {
                let cidr: IpCidr = s[col_idx + 1..]
                    .parse()
                    .map_err(|e| Error(format!("{} not a valid CIDR: {}", &s[col_idx + 1..], e)))?;
                Ok((cidr, s[..col_idx].to_string()))
            }
        }
    }

    fn bandwidth_parse(s: &str) -> Result<Option<NonZeroU64>, Error> {
        let s = s.trim();
        let multiplier_b = s
            .as_bytes()
            .last()
            .ok_or_else(|| Error(format!("\"{}\" bandwidth specifier empty", s)))?;
        let multiplier_order = match multiplier_b {
            b'k' | b'K' => 1u32,
            b'm' | b'M' => 2,
            b'g' | b'G' => 3,
            b't' | b'T' => 4,
            b'p' | b'P' => 5,
            b'e' | b'E' => 6,
            _ => 0,
        };
        let (multiplier, s) = match multiplier_order {
            0 => (1u64, s),
            mo => {
                let base: u64 = if (*multiplier_b as char).is_uppercase() {
                    1024
                } else {
                    1000
                };
                (base.pow(mo), &s[..s.len() - 1])
            }
        };

        let number =
            u64::from_str(s).map_err(|e| Error(format!("\"{}\" not bandwidth size: {}", s, e)))?;
        Ok(NonZeroU64::new(number.checked_mul(multiplier).ok_or_else(
            || Error(format!("{} * {} too big", number, multiplier)),
        )?))
    }

    fn mime_type_override_parse(s: &str) -> Result<(OsString, String), Error> {
        match s.find(':') {
            None => Err(Error(format!("{} not in EXTENSION:MIME-TYPE format", s))),
            Some(col_idx) => {
                let mime_s = &s[col_idx + 1..];
                // Basic MIME type validation
                if !mime_s.contains('/') {
                    return Err(Error(format!("{} not a valid MIME type", mime_s)));
                }
                Ok((OsString::from(&s[..col_idx]), mime_s.to_string()))
            }
        }
    }

    fn header_parse(s: &str) -> Result<(String, Vec<u8>), Error> {
        s.split_once(':')
            .and_then(|(hn, mut hd)| {
                hd = hd.trim_start();
                if !hn.is_empty() && !hd.is_empty() {
                    Some((hn, hd))
                } else {
                    None
                }
            })
            .map(|(hn, hd)| (hn.to_string(), hd.as_bytes().to_vec()))
            .ok_or_else(|| Error(format!("\"{}\" invalid header format", s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_settings_that_cannot_be_enforced() {
        let dir = crate::test_support::TestDir::new();
        for flags in [
            vec!["--proxy", "X-Forwarded-For:127.0.0.0/8"],
            vec!["--proxy-redir", "X-Original-URL:127.0.0.0/8"],
            vec!["--request-bandwidth", "1"],
        ] {
            assert!(
                Options::parse_test(dir.path(), &flags)
                    .unwrap_err()
                    .0
                    .contains("not implemented")
            );
        }
        assert!(Options::parse_test(dir.path(), &["--request-bandwidth", "0"]).is_ok());
    }

    #[test]
    fn instance_cleanup_does_not_remove_another_instances_files() {
        let dir = crate::test_support::TestDir::new();
        let flags = ["-t", dir.path().to_str().unwrap()];
        let first = Options::parse_test(dir.path(), &flags).unwrap();
        let second = Options::parse_test(dir.path(), &flags).unwrap();
        assert_ne!(first.temp_directory.1, second.temp_directory.1);
        let guard = crate::InstanceTemp::create(&first.temp_directory.1).unwrap();
        let other = crate::InstanceTemp::create(&second.temp_directory.1).unwrap();
        std::fs::write(second.temp_directory.1.join("keep"), "other instance").unwrap();
        drop(guard);
        assert!(second.temp_directory.1.join("keep").is_file());
        assert!(crate::InstanceTemp::create(&second.temp_directory.1).is_err());
        drop(other);
    }
}
