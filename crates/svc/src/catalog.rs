//! The tweak catalog compiled into the service.
//!
//! The catalog is embedded rather than read from disk: what the service may
//! change as LocalSystem is then fixed by the binary, and there is no
//! catalog file whose permissions could be gotten wrong.

use syspilot_proto::catalog::{Catalog, CatalogError};

/// The catalog files, in the order their categories are shown.
const SOURCES: &[&str] = &[
    include_str!("../../../catalog/tweaks/performance.toml"),
    include_str!("../../../catalog/tweaks/privacy.toml"),
    include_str!("../../../catalog/tweaks/security.toml"),
    include_str!("../../../catalog/tweaks/reliability.toml"),
    include_str!("../../../catalog/tweaks/other.toml"),
];

/// Parses the embedded catalog.
pub fn builtin() -> Result<Catalog, CatalogError> {
    Catalog::parse_all(SOURCES.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn embedded_catalog_is_valid_and_not_empty() {
        let catalog = builtin().unwrap();
        assert!(catalog.tweaks.len() >= 50, "received: {} tweaks", catalog.tweaks.len());
    }

    /// A catalog file that is added to the directory but not to `SOURCES`
    /// would silently be missing from the product.
    #[test]
    fn every_catalog_file_is_embedded() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog/tweaks");
        let mut on_disk = 0;
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                // Compare without line endings, which git may convert.
                let text = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
                let embedded = SOURCES.iter().any(|source| source.replace("\r\n", "\n") == text);
                assert!(embedded, "(SOURCES): expected {} to be embedded", path.display());
                on_disk += 1;
            }
        }
        assert_eq!(on_disk, SOURCES.len(), "SOURCES names a file that is not in the directory");
    }
}
