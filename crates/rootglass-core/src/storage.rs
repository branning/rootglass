use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSelection {
    pub read_only_rootfs: PathBuf,
    pub writable_overlay: Option<WritableOverlaySelection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritableOverlaySelection {
    Bundled(PathBuf),
    Local(LocalWritableOverlay),
}

impl WritableOverlaySelection {
    pub fn path(&self) -> &Path {
        match self {
            Self::Bundled(path) => path.as_path(),
            Self::Local(overlay) => overlay.path.as_path(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWritableOverlay {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayStore {
    root: PathBuf,
}

impl OverlayStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn glass_overlay_dir(&self, glass_identifier: &str) -> PathBuf {
        self.root.join(sanitize_identifier(glass_identifier))
    }

    pub fn overlay_path(&self, glass_identifier: &str, overlay_name: &str) -> PathBuf {
        self.glass_overlay_dir(glass_identifier)
            .join(format!("{}.img", sanitize_identifier(overlay_name)))
    }

    pub fn selection_for(
        &self,
        glass_root: &Path,
        glass_identifier: &str,
        read_only_rootfs: &str,
        bundled_overlay: Option<&str>,
        local_overlay_name: Option<&str>,
    ) -> StorageSelection {
        let read_only_rootfs = glass_root.join(read_only_rootfs);
        let writable_overlay = match local_overlay_name {
            Some(name) => Some(WritableOverlaySelection::Local(LocalWritableOverlay {
                name: name.to_string(),
                path: self.overlay_path(glass_identifier, name),
            })),
            None => bundled_overlay.map(|path| WritableOverlaySelection::Bundled(glass_root.join(path))),
        };

        StorageSelection {
            read_only_rootfs,
            writable_overlay,
        }
    }
}

fn sanitize_identifier(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_local_overlay_when_selected() {
        let store = OverlayStore::new("/tmp/rootglass/overlays");
        let selection = store.selection_for(
            Path::new("/tmp/hello-world.glass"),
            "io.rootglass.examples.hello-world",
            "rootfs.img",
            Some("state.img"),
            Some("experiment-1"),
        );

        assert_eq!(selection.read_only_rootfs, PathBuf::from("/tmp/hello-world.glass/rootfs.img"));
        let overlay = selection.writable_overlay.expect("overlay selected");
        assert_eq!(overlay.path(), Path::new("/tmp/rootglass/overlays/io.rootglass.examples.hello-world/experiment-1.img"));
    }

    #[test]
    fn falls_back_to_bundled_overlay_when_no_local_overlay_is_selected() {
        let store = OverlayStore::new("/tmp/rootglass/overlays");
        let selection = store.selection_for(
            Path::new("/tmp/hello-world.glass"),
            "io.rootglass.examples.hello-world",
            "rootfs.img",
            Some("state.img"),
            None,
        );

        let overlay = selection.writable_overlay.expect("bundled overlay selected");
        assert_eq!(overlay.path(), Path::new("/tmp/hello-world.glass/state.img"));
    }
}
