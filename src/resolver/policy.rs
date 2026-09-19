//! What a mapper may point the server at.
//!
//! A mapper is a trust boundary: whatever it returns is validated here before anything is
//! opened. File locations must be plain relative paths; remote locations must satisfy the
//! operator's host allow-list and address policy.

use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

use reqwest::Url;

use crate::config::RemoteMediaConfig;
use crate::source::is_public_address;

const MAX_PATH_BYTES: usize = 4096;

/// The operator's rules for remote media locations.
#[derive(Debug, Clone)]
pub(crate) struct LocationPolicy {
    allowed_hosts: Vec<String>,
    allow_insecure_http: bool,
    allow_private_addresses: bool,
}

impl LocationPolicy {
    pub(crate) fn new(config: &RemoteMediaConfig) -> Self {
        Self {
            allowed_hosts: config
                .allowed_hosts
                .iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
            allow_insecure_http: config.allow_insecure_http,
            allow_private_addresses: config.allow_private_addresses,
        }
    }

    /// Checks a URL before any request is made to it.
    pub(crate) fn check_url(&self, url: &Url) -> Result<(), String> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_insecure_http => {}
            other => return Err(format!("location scheme `{other}` is not permitted")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("location must not contain credentials".to_owned());
        }
        let host = url
            .host_str()
            .ok_or_else(|| "location has no host".to_owned())?
            .to_ascii_lowercase();
        // `host_str` keeps the brackets on IPv6 literals.
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if !self.allowed_hosts.iter().any(|allowed| allowed == bare) {
            return Err("location host is not in remote_media.allowed_hosts".to_owned());
        }
        // Name resolution is filtered at connect time, but a literal address skips it.
        if let Ok(address) = bare.parse::<IpAddr>()
            && !self.allow_private_addresses
            && !is_public_address(address)
        {
            return Err("location address is not permitted".to_owned());
        }
        Ok(())
    }
}

/// Accepts only a plain relative path: no root, no `..`, no NUL, bounded length.
pub(crate) fn validate_relative_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err("location path is empty, too long, or contains NUL".to_owned());
    }
    let path = Path::new(path);
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("location path must be relative and free of `.` and `..`".to_owned());
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(hosts: &[&str], insecure: bool, private: bool) -> LocationPolicy {
        LocationPolicy::new(&RemoteMediaConfig {
            allowed_hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            allow_insecure_http: insecure,
            allow_private_addresses: private,
            ..RemoteMediaConfig::default()
        })
    }

    fn check(policy: &LocationPolicy, url: &str) -> Result<(), String> {
        policy.check_url(&Url::parse(url).unwrap())
    }

    #[test]
    fn accepts_allow_listed_https_hosts_case_insensitively() {
        let policy = policy(&["origin.example.net"], false, false);
        check(&policy, "https://Origin.Example.NET/a/b.mp4?sig=1").unwrap();
    }

    #[test]
    fn rejects_everything_outside_the_policy() {
        let policy = policy(&["origin.example.net"], false, false);
        for url in [
            "http://origin.example.net/a.mp4",
            "ftp://origin.example.net/a.mp4",
            "file:///etc/passwd",
            "https://other.example.net/a.mp4",
            "https://origin.example.net.evil.test/a.mp4",
            "https://user:pw@origin.example.net/a.mp4",
            "https://127.0.0.1/a.mp4",
            "https://[::1]/a.mp4",
        ] {
            assert!(check(&policy, url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn empty_allow_list_refuses_all_hosts() {
        assert!(
            check(
                &policy(&[], false, false),
                "https://origin.example.net/a.mp4"
            )
            .is_err()
        );
    }

    #[test]
    fn insecure_and_private_locations_need_explicit_opt_in() {
        let allowed = policy(&["127.0.0.1", "localhost"], true, true);
        check(&allowed, "http://127.0.0.1:9000/a.mp4").unwrap();
        check(&allowed, "http://localhost:9000/a.mp4").unwrap();
        // A literal private address is still refused unless private addresses are allowed.
        let strict = policy(&["10.0.0.5"], true, false);
        assert!(check(&strict, "http://10.0.0.5/a.mp4").is_err());
    }

    #[test]
    fn validates_relative_paths() {
        assert_eq!(
            validate_relative_path("movies/a.mp4").unwrap(),
            PathBuf::from("movies/a.mp4")
        );
        for path in [
            "",
            "/etc/passwd",
            "../a.mp4",
            "a/../../b.mp4",
            "./a.mp4",
            "a\0b",
            &"x".repeat(5000),
        ] {
            assert!(
                validate_relative_path(path).is_err(),
                "{path:?} should be refused"
            );
        }
    }
}
