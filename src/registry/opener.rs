//! Turns a resolved location into an open media source.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::resolver::AssetLocation;
use crate::source::{LocalMediaSource, MediaSourceKind, RemoteReader};

#[derive(Debug)]
pub(crate) struct SourceOpener {
    media_root: PathBuf,
    remote: RemoteReader,
}

impl SourceOpener {
    pub(crate) fn new(media_root: PathBuf, remote: RemoteReader) -> Self {
        Self { media_root, remote }
    }

    pub(crate) async fn open(&self, location: &AssetLocation) -> Result<MediaSourceKind> {
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
                Ok(MediaSourceKind::Http(Arc::new(source)))
            }
        }
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
