//! The tweak catalog compiled into the service.
//!
//! The catalog is embedded rather than read from disk: what the service may
//! change as LocalSystem is then fixed by the signed binary, and there is
//! no catalog file whose permissions could be gotten wrong.

use syspilot_proto::catalog::{Catalog, CatalogError};

/// The catalog files, in the order their tweaks are listed.
const SOURCES: &[&str] = &[];

/// Parses the embedded catalog.
pub fn builtin() -> Result<Catalog, CatalogError> {
    Catalog::parse_all(SOURCES.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_is_valid() {
        let catalog = builtin().unwrap();
        let mut ids: Vec<&str> = catalog.tweaks.iter().map(|tweak| tweak.id.as_str()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "tweak IDs must be unique across catalog files");
    }
}
