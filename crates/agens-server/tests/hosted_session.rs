use agens_core::SessionMetadata;
use agens_core::hosted::{
    CatalogEntry, CatalogKind, CatalogResult, CatalogSnapshot, FileError, HostedCatalogs,
    HostedTaskJournal, HostedTaskLimits, HostedTaskReplay, HostedTaskState, HostedWorkspaceFiles,
    MAX_WORKSPACE_FILE_BYTES, MAX_WORKSPACE_FILE_ENTRIES, WorkspaceFileContent, WorkspaceFileKind,
};
use agens_server::{ConfinedWorkspaceFiles, HostedCatalogSet};
use agens_store::{HostedTaskStore, SessionStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agens-hosted-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn catalog_snapshots_report_stale_and_unsupported_without_client_fallback() {
    let catalogs = HostedCatalogSet::new(
        Some(CatalogSnapshot::new(
            "commands-v2",
            vec![CatalogEntry::new("/help", "Show help", true)],
        )),
        None,
    );

    assert!(matches!(
        catalogs.catalog(CatalogKind::Command, None),
        CatalogResult::Current(snapshot) if snapshot.revision() == "commands-v2"
    ));
    assert_eq!(
        catalogs.catalog(CatalogKind::Command, Some("commands-v1")),
        CatalogResult::Stale {
            current_revision: "commands-v2".into()
        }
    );
    assert_eq!(
        catalogs.catalog(CatalogKind::Skill, None),
        CatalogResult::Unsupported
    );
}

#[test]
fn catalogs_files_threat_matrix_is_data_only_and_confined() {
    let root = Scratch::new();
    fs::write(root.path().join("requirements.txt"), "requests==2\n").unwrap();
    fs::write(
        root.path().join("CMakeLists.txt"),
        "execute_process(COMMAND false)\n",
    )
    .unwrap();
    fs::write(root.path().join("guide.md"), "#!/bin/sh\nfalse\n").unwrap();
    fs::write(
        root.path().join("component.mdx"),
        "export const run = () => false\n",
    )
    .unwrap();
    fs::write(root.path().join("README.sh"), "#!/bin/sh\nfalse\n").unwrap();

    let files = ConfinedWorkspaceFiles::default();
    for path in [
        "requirements.txt",
        "CMakeLists.txt",
        "guide.md",
        "component.mdx",
        "README.sh",
    ] {
        let content = files.read(root.path(), Path::new(path)).unwrap();
        assert!(
            matches!(content, WorkspaceFileContent::Text { .. }),
            "{path}"
        );
    }

    for selector in ["git -C /tmp status", "../outside", "/etc/passwd"] {
        assert!(
            matches!(
                files.read(root.path(), Path::new(selector)),
                Err(FileError::InvalidSelector | FileError::OutsideRoot)
            ),
            "{selector}"
        );
    }
}

#[test]
fn files_are_ignore_aware_and_enforce_entry_and_byte_limits() {
    let root = Scratch::new();
    fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(root.path().join("visible.txt"), "visible").unwrap();
    fs::write(root.path().join("ignored.txt"), "secret").unwrap();
    fs::write(
        root.path().join("large.txt"),
        vec![b'x'; MAX_WORKSPACE_FILE_BYTES + 1],
    )
    .unwrap();

    let files = ConfinedWorkspaceFiles::default();
    let listed = files.list(root.path(), Path::new(".")).unwrap();
    assert!(
        listed
            .iter()
            .any(|entry| entry.path() == Path::new("visible.txt"))
    );
    assert!(
        !listed
            .iter()
            .any(|entry| entry.path() == Path::new("ignored.txt"))
    );
    assert_eq!(
        files.read(root.path(), Path::new("ignored.txt")),
        Err(FileError::Ignored)
    );
    assert_eq!(
        files.read(root.path(), Path::new("large.txt")),
        Err(FileError::Oversized)
    );

    let crowded = root.path().join("crowded");
    fs::create_dir(&crowded).unwrap();
    for index in 0..=MAX_WORKSPACE_FILE_ENTRIES {
        fs::write(crowded.join(format!("{index:04}.txt")), "x").unwrap();
    }
    assert_eq!(
        files.list(root.path(), Path::new("crowded")),
        Err(FileError::EntryLimit)
    );
}

#[test]
fn reads_distinguish_text_media_unsupported_and_escape() {
    let root = Scratch::new();
    let outside = Scratch::new();
    fs::write(root.path().join("plain"), "utf8").unwrap();
    fs::write(root.path().join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(root.path().join("binary.bin"), b"a\0b").unwrap();
    fs::write(outside.path().join("outside.txt"), "outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("outside.txt"),
        root.path().join("escape.txt"),
    )
    .unwrap();

    let files = ConfinedWorkspaceFiles::default();
    assert!(matches!(
        files.read(root.path(), Path::new("plain")),
        Ok(WorkspaceFileContent::Text { .. })
    ));
    assert!(matches!(
        files.read(root.path(), Path::new("image.png")),
        Ok(WorkspaceFileContent::Media {
            kind: WorkspaceFileKind::Media,
            ..
        })
    ));
    assert_eq!(
        files.read(root.path(), Path::new("binary.bin")),
        Err(FileError::Unsupported)
    );
    #[cfg(unix)]
    assert_eq!(
        files.read(root.path(), Path::new("escape.txt")),
        Err(FileError::OutsideRoot)
    );
}

#[cfg(unix)]
#[test]
fn reads_reject_a_replaced_root_symlink() {
    let outside = Scratch::new();
    let root = outside.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::remove_dir(&root).unwrap();
    std::os::unix::fs::symlink(outside.path(), &root).unwrap();
    assert_eq!(
        ConfinedWorkspaceFiles::default().read(&root, Path::new("file.txt")),
        Err(FileError::OutsideRoot)
    );
}

#[test]
fn task_replay_runtime_harness_restores_snapshot_tail_and_child_turns() {
    let scratch = Scratch::new();
    SessionStore::open(scratch.path())
        .unwrap()
        .open_session(&SessionMetadata {
            id: 41,
            project: "project".into(),
            title: "Hosted".into(),
            active_agent: "general".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        })
        .unwrap();
    let mut store =
        HostedTaskStore::open_with_limits(scratch.path(), HostedTaskLimits::with_limits(2, 10))
            .unwrap();
    store
        .append_event(41, "child", HostedTaskState::Running, "start")
        .unwrap();
    store
        .append_event(41, "child", HostedTaskState::Background, "background")
        .unwrap();
    store
        .persist_completed_child_turn(41, "child", 1, "completed-turn")
        .unwrap();
    store
        .append_event(41, "child", HostedTaskState::Completed, "done")
        .unwrap();

    let journal: Box<dyn HostedTaskJournal> = Box::new(store);
    let HostedTaskReplay::SnapshotTail { snapshot, events } = journal.snapshot_tail(41).unwrap()
    else {
        panic!("snapshot tail expected");
    };
    assert_eq!(snapshot.cursor(), 1);
    assert_eq!(snapshot.child_turns()[0].payload(), "completed-turn");
    assert_eq!(events.len(), 2);
}

#[test]
fn hosted_mcp_controls_return_resulting_status_without_configuration_reload() {
    use agens_core::hosted::{
        HostedMcpAction, HostedMcpControl, HostedMcpResult, HostedMcpServer, HostedMcpState,
    };

    #[derive(Default)]
    struct Mcp {
        generation: u64,
        state: HostedMcpState,
    }
    impl HostedMcpControl for Mcp {
        fn status(&self) -> Vec<HostedMcpServer> {
            vec![HostedMcpServer::new(
                "files",
                self.state,
                self.generation,
                None,
            )]
        }
        fn control(&mut self, server: &str, action: HostedMcpAction) -> HostedMcpResult {
            assert_eq!(server, "files");
            self.generation += 1;
            self.state = match action {
                HostedMcpAction::Connect | HostedMcpAction::Reconnect => HostedMcpState::Ready,
                HostedMcpAction::Disconnect => HostedMcpState::Closed,
            };
            HostedMcpResult::new(self.status(), None)
        }
    }

    let mut mcp = Mcp::default();
    let connected = mcp.control("files", HostedMcpAction::Connect);
    assert_eq!(connected.servers()[0].state(), HostedMcpState::Ready);
    let disconnected = mcp.control("files", HostedMcpAction::Disconnect);
    assert_eq!(disconnected.servers()[0].state(), HostedMcpState::Closed);
    let reconnected = mcp.control("files", HostedMcpAction::Reconnect);
    assert_eq!(reconnected.servers()[0].generation(), 3);
}
