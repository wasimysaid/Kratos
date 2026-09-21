use std::fs;

use rusqlite::{Connection, params};
use kratos_engine::peer_auth::{AuthStore, DeviceIdentity};
use kratos_engine::peer_runtime::backup::{
    BackupError, TRUST_ROLLBACK_WARNING, create_generation, publish_directory, restore_generation,
    verify_generation,
};
use kratos_sync::peer::PeerStore;

fn seed_host(root: &std::path::Path) -> (AuthStore, DeviceIdentity, String, String, PeerStore) {
    fs::create_dir_all(root.join("peer")).unwrap();
    let identity = DeviceIdentity::load_or_create(root.join("peer-device.json")).unwrap();
    let auth = AuthStore::open(root.join("peer/auth.sqlite")).unwrap();
    let principal = auth
        .create_profile(&identity.public_key(), Some("recovery host"))
        .unwrap();
    let address = format!("tc{}", "a".repeat(80));
    fs::write(
        root.join("peer-session.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "profileId": principal.profile_id,
            "deviceId": principal.device_id,
            "address": address,
            "hosting": true,
            "localPort": 27191,
            "derpMap": null
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("peer/tailcat-server.key"),
        br#"{"role":"server","private_key":"secret","preshared_key":"pair","region":{"RegionID":1}}"#,
    )
    .unwrap();

    let peer = PeerStore::open(root.join("peer/peer.sqlite")).unwrap();
    let db = Connection::open(root.join("peer/peer.sqlite")).unwrap();
    db.execute(
        "INSERT INTO chat_checkpoints(profile,chat,seq_covered,frontier,bytes,committed_at) VALUES(?1,'chat-recovery',7,?2,?3,1)",
        params![principal.profile_id, b"frontier", b"durable transcript"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO sidecars(profile,scope,owner,name,content_type,bytes,updated_at) VALUES(?1,'tool','chat-recovery','result','text/plain',?2,1)",
        params![principal.profile_id, b"durable tool output"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO attachment_uploads(profile,upload,sender,target,file_name,length,digest,committed,created_at,committed_at) VALUES(?1,'upload-1',?2,'viewer','proof.txt',16,'digest',1,1,2)",
        params![principal.profile_id, principal.device_id],
    )
    .unwrap();
    db.execute(
        "INSERT INTO attachment_chunks(profile,upload,sender,target,offset,bytes) VALUES(?1,'upload-1',?2,'viewer',0,?3)",
        params![principal.profile_id, principal.device_id, b"attachment bytes"],
    )
    .unwrap();
    drop(db);
    (
        auth,
        identity,
        principal.profile_id,
        principal.device_id,
        peer,
    )
}

#[test]
fn complete_generation_restores_trust_docs_and_attachment_custody() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let (auth, identity, profile, device, peer) = seed_host(&source);

    // The auth connection remains live while both SQLite snapshots are made.
    let generation = create_generation(&source, &peer).unwrap();
    let manifest = verify_generation(&generation).unwrap();
    assert_eq!(manifest.profile_id, profile);
    assert_eq!(manifest.device_id, device);
    assert_eq!(manifest.trust_rollback_warning, TRUST_ROLLBACK_WARNING);
    assert_eq!(manifest.files.len(), 5);
    assert!(manifest.files.iter().all(|f| !f.path.ends_with("-wal")));

    let restored = temp.path().join("restored");
    assert!(matches!(
        restore_generation(&generation, &restored, false),
        Err(BackupError::TrustRollbackNotAcknowledged)
    ));
    restore_generation(&generation, &restored, true).unwrap();

    let restored_identity =
        DeviceIdentity::load_or_create(restored.join("peer-device.json")).unwrap();
    assert_eq!(restored_identity.public_key(), identity.public_key());
    let restored_auth = AuthStore::open(restored.join("peer/auth.sqlite")).unwrap();
    let challenge = restored_auth.create_challenge(&profile, &device).unwrap();
    let token = restored_auth
        .authenticate_challenge(&restored_identity.sign_challenge(&challenge).unwrap())
        .unwrap();
    assert_eq!(
        restored_auth.authenticate(&token.token).unwrap().device_id,
        device
    );

    let db = Connection::open(restored.join("peer/peer.sqlite")).unwrap();
    let checkpoint: Vec<u8> = db
        .query_row(
            "SELECT bytes FROM chat_checkpoints WHERE profile=?1 AND chat='chat-recovery'",
            [&profile],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(checkpoint, b"durable transcript");
    let attachment: Vec<u8> = db
        .query_row(
            "SELECT bytes FROM attachment_chunks WHERE profile=?1 AND upload='upload-1'",
            [&profile],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attachment, b"attachment bytes");
    drop(auth);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&generation).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for entry in manifest.files {
            assert_eq!(
                fs::metadata(generation.join(entry.path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn restore_rejects_tampering_missing_secrets_and_nonempty_targets() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let (_auth, _identity, _profile, _device, peer) = seed_host(&source);

    let tampered = create_generation(&source, &peer).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(tampered.join("peer.sqlite"))
        .unwrap()
        .write_all(b"tamper")
        .unwrap();
    assert!(matches!(
        verify_generation(&tampered),
        Err(BackupError::Invalid(_))
    ));

    let missing = create_generation(&source, &peer).unwrap();
    fs::remove_file(missing.join("tailcat-server.key")).unwrap();
    assert!(verify_generation(&missing).is_err());

    let unsafe_manifest = create_generation(&source, &peer).unwrap();
    let path = unsafe_manifest.join("manifest.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    json["files"][0]["path"] = serde_json::json!("../peer.sqlite");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(matches!(
        verify_generation(&unsafe_manifest),
        Err(BackupError::Invalid(_))
    ));

    let valid = create_generation(&source, &peer).unwrap();
    let occupied = temp.path().join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("unrelated"), b"keep").unwrap();
    assert!(matches!(
        restore_generation(&valid, &occupied, true),
        Err(BackupError::TargetNotEmpty)
    ));
    assert_eq!(fs::read(occupied.join("unrelated")).unwrap(), b"keep");
}
#[test]
fn publication_validates_and_prepares_source_before_move() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("stage");
    let destination = temp.path().join("published");

    fs::write(&source, b"not a directory").unwrap();
    let error = publish_directory(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(&source).unwrap(), b"not a directory");
    assert!(!destination.exists());

    fs::remove_file(&source).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("entry"), b"durable").unwrap();
    publish_directory(&source, &destination).unwrap();
    assert!(!source.exists());
    assert_eq!(fs::read(destination.join("entry")).unwrap(), b"durable");
}

use std::io::Write as _;
