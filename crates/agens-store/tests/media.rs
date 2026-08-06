use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_store::{
    MAX_MEDIA_BYTES, MediaStoreError, SessionStore, guess_mime_from_bytes, guess_mime_from_path,
    ingest_media_bytes, ingest_media_path, is_media_mime, media_chip_label, open_media,
};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("agens-store-media-{}-{suffix}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn ingest_bytes_writes_content_addressed_blob_and_index_row() {
    let directory = data_directory();
    let _store = SessionStore::open(&directory).unwrap();
    let bytes = b"png-bytes-fixture";

    let record = ingest_media_bytes(&directory, bytes, "image/png").unwrap();

    assert!(record.id > 0);
    assert_eq!(record.mime, "image/png");
    assert_eq!(record.byte_len, bytes.len() as u64);
    assert_eq!(record.sha256.len(), 64);

    let blob_path = directory.join("media").join(&record.sha256);
    assert_eq!(fs::read(&blob_path).unwrap(), bytes);

    let connection = Connection::open(directory.join("agens.db")).unwrap();
    let (sha256, mime, byte_len): (String, String, i64) = connection
        .query_row(
            "SELECT sha256, mime, byte_len FROM media WHERE id = ?1",
            [record.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(sha256, record.sha256);
    assert_eq!(mime, "image/png");
    assert_eq!(byte_len, bytes.len() as i64);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ingest_path_reads_file_and_deduplicates_by_content_hash() {
    let directory = data_directory();
    let _store = SessionStore::open(&directory).unwrap();
    let source = directory.join("source.bin");
    fs::write(&source, b"same-bytes").unwrap();

    let first = ingest_media_path(&directory, &source, "image/jpeg").unwrap();
    let second = ingest_media_bytes(&directory, b"same-bytes", "image/jpeg").unwrap();

    assert_eq!(first.sha256, second.sha256);
    assert_eq!(first.id, second.id);
    assert_eq!(
        fs::read_dir(directory.join("media"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_file())
            .count(),
        1
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn oversize_ingest_is_rejected_before_durable_write() {
    let directory = data_directory();
    let _store = SessionStore::open(&directory).unwrap();
    let bytes = vec![0_u8; MAX_MEDIA_BYTES + 1];

    let error = ingest_media_bytes(&directory, &bytes, "image/png").unwrap_err();
    assert!(matches!(error, MediaStoreError::Oversize { .. }));

    assert!(
        !directory.join("media").exists() || {
            fs::read_dir(directory.join("media"))
                .map(|entries| entries.count() == 0)
                .unwrap_or(true)
        }
    );
    let connection = Connection::open(directory.join("agens.db")).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM media", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn open_media_returns_mime_and_content_addressed_path() {
    let directory = data_directory();
    let _store = SessionStore::open(&directory).unwrap();
    let record = ingest_media_bytes(&directory, b"open-me", "image/webp").unwrap();

    let (mime, path) = open_media(&directory, record.id).unwrap();

    assert_eq!(mime, "image/webp");
    assert_eq!(path, directory.join("media").join(&record.sha256));
    assert_eq!(fs::read(&path).unwrap(), b"open-me");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn open_unknown_media_id_fails() {
    let directory = data_directory();
    let _store = SessionStore::open(&directory).unwrap();

    let error = open_media(&directory, 999).unwrap_err();
    assert!(matches!(error, MediaStoreError::NotFound { media_id: 999 }));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn guess_mime_from_path_maps_common_image_and_pdf_extensions() {
    assert_eq!(
        guess_mime_from_path(Path::new("photo.PNG")).as_deref(),
        Some("image/png")
    );
    assert_eq!(
        guess_mime_from_path(Path::new("shot.jpeg")).as_deref(),
        Some("image/jpeg")
    );
    assert_eq!(
        guess_mime_from_path(Path::new("doc.pdf")).as_deref(),
        Some("application/pdf")
    );
    assert_eq!(guess_mime_from_path(Path::new("notes.txt")), None);
}

#[test]
fn guess_mime_from_bytes_detects_png_and_jpeg_magic() {
    let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 1, 2];
    assert_eq!(guess_mime_from_bytes(&png).as_deref(), Some("image/png"));
    let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 0];
    assert_eq!(guess_mime_from_bytes(&jpeg).as_deref(), Some("image/jpeg"));
    assert_eq!(guess_mime_from_bytes(b"not-an-image"), None);
}

#[test]
fn media_chip_label_is_path_free_and_1_based() {
    assert_eq!(media_chip_label(1, "image/png"), "[Image #1]");
    assert_eq!(media_chip_label(2, "image/jpeg"), "[Image #2]");
    assert_eq!(media_chip_label(1, "application/pdf"), "[File #1]");
    assert!(is_media_mime("image/webp"));
    assert!(is_media_mime("application/pdf"));
    assert!(!is_media_mime("text/plain"));
}
