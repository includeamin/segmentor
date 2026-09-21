//! Turns a resolved location into an open media source.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use reqwest::Url;

use crate::error::{Error, Result};
use crate::resolver::AssetLocation;
use crate::source::{
    ByteRange, LocalMediaSource, LocationRefresher, MediaSourceKind, RemoteReader,
};

#[derive(Debug)]
pub(crate) struct SourceOpener {
    media_root: PathBuf,
    remote: RemoteReader,
}

impl SourceOpener {
    pub(crate) fn new(media_root: PathBuf, remote: RemoteReader) -> Self {
        Self { media_root, remote }
    }

    /// Opens `location`. A remote source is given `refresher` so it can recover a rejected URL.
    pub(crate) async fn open(
        &self,
        location: &AssetLocation,
        refresher: Arc<dyn LocationRefresher>,
    ) -> Result<MediaSourceKind> {
        match location {
            AssetLocation::File(path) => {
                let root = self.media_root.clone();
                let path = path.clone();
                let source = tokio::task::spawn_blocking(move || {
                    let confined = confine(&root, &path)?;
                    LocalMediaSource::open(confined)
                })
                .await
                .map_err(|error| Error::Io(std::io::Error::other(error)))??;
                Ok(MediaSourceKind::Local(Arc::new(source)))
            }
            AssetLocation::Http(url) => {
                let source = self.remote.open(url.clone()).await?;
                source.set_refresher(refresher);
                Ok(MediaSourceKind::Http(Arc::new(source)))
            }
        }
    }
}

impl SourceOpener {
    /// Reads a whole small object, such as a subtitle file, refusing one over `max_bytes` before
    /// any of it is read. It goes through the same confinement and remote-media rules as media.
    pub(crate) async fn read_whole(
        &self,
        location: &AssetLocation,
        max_bytes: u64,
    ) -> Result<Bytes> {
        let source = match location {
            AssetLocation::File(_) => self.open(location, Arc::new(NoRefresh)).await?,
            AssetLocation::Http(url) => {
                MediaSourceKind::Http(Arc::new(self.remote.open(url.clone()).await?))
            }
        };
        let length = source.len();
        if length > max_bytes {
            return Err(Error::InvalidMedia(format!(
                "file is {length} bytes, more than the limit of {max_bytes}"
            )));
        }
        source.read_range(ByteRange::new(0, length)).await
    }
}

/// For an object that has no signed URL to renew.
#[derive(Debug)]
struct NoRefresh;

impl LocationRefresher for NoRefresh {
    fn refresh(&self) -> Pin<Box<dyn Future<Output = Result<Url>> + Send + '_>> {
        Box::pin(async {
            Err(Error::Upstream(
                "this location cannot be refreshed".to_owned(),
            ))
        })
    }
}

/// Joins `path` to the media root, resolves symlinks, and requires a regular file that is still
/// beneath the root, so no location can escape it.
fn confine(root: &Path, path: &Path) -> Result<PathBuf> {
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|error| Error::Upstream(format!("location cannot be opened: {error}")))?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return Err(Error::Upstream(
            "location does not resolve to a file beneath storage.media_root".to_owned(),
        ));
    }
    Ok(resolved)
}
