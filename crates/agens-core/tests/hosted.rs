use agens_core::hosted::{
    CatalogEntry, CatalogKind, CatalogResult, CatalogSnapshot, FileError, HostedCatalogs,
    HostedWorkspaceFiles, WorkspaceFileKind,
};
use std::path::Path;

struct Catalogs(CatalogSnapshot);

impl HostedCatalogs for Catalogs {
    fn catalog(&self, kind: CatalogKind, known_revision: Option<&str>) -> CatalogResult {
        if kind == CatalogKind::Skill {
            return CatalogResult::Unsupported;
        }
        self.0.resolve(known_revision)
    }
}

struct NoFiles;

impl HostedWorkspaceFiles for NoFiles {
    fn list(
        &self,
        _: &Path,
        _: &Path,
    ) -> Result<Vec<agens_core::hosted::WorkspaceFile>, FileError> {
        Err(FileError::Unsupported)
    }

    fn read(
        &self,
        _: &Path,
        _: &Path,
    ) -> Result<agens_core::hosted::WorkspaceFileContent, FileError> {
        Err(FileError::Unsupported)
    }
}

#[test]
fn catalogs_expose_current_stale_and_unsupported_outcomes() {
    let catalogs = Catalogs(CatalogSnapshot::new(
        "rev-2",
        vec![CatalogEntry::new("/help", "Show help", true)],
    ));

    assert!(matches!(
        catalogs.catalog(CatalogKind::Command, None),
        CatalogResult::Current(snapshot) if snapshot.revision() == "rev-2"
    ));
    assert_eq!(
        catalogs.catalog(CatalogKind::Command, Some("rev-1")),
        CatalogResult::Stale {
            current_revision: "rev-2".into()
        }
    );
    assert_eq!(
        catalogs.catalog(CatalogKind::Skill, None),
        CatalogResult::Unsupported
    );
}

#[test]
fn file_contracts_preserve_typed_bounds_and_unsupported_results() {
    assert_eq!(agens_core::hosted::MAX_WORKSPACE_FILE_ENTRIES, 2_000);
    assert_eq!(agens_core::hosted::MAX_WORKSPACE_FILE_BYTES, 1024 * 1024);
    let command_shaped_file =
        agens_core::hosted::WorkspaceFile::new("README.sh".into(), 12, WorkspaceFileKind::Text);
    assert_eq!(command_shaped_file.path(), Path::new("README.sh"));
    assert_eq!(command_shaped_file.kind(), WorkspaceFileKind::Text);

    let files = NoFiles;
    assert_eq!(
        files.list(Path::new("."), Path::new(".")),
        Err(FileError::Unsupported)
    );
    assert_eq!(
        files.read(Path::new("."), Path::new("README.md")),
        Err(FileError::Unsupported)
    );
}
