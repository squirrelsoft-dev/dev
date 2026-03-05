//! Unit tests for session module

use buildkit_client::session::{Session, FileSyncServer, AuthServer, RegistryAuthConfig};

#[test]
fn test_session_creation() {
    let session = Session::new();

    // Session ID should be a valid UUID
    assert!(!session.id.is_empty());
    // UUID v4 format with hyphens: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    assert!(session.id.len() == 36 || session.id.len() == 44); // With or without braces

    // Shared key should be a valid UUID
    assert!(!session.shared_key.is_empty());
    assert!(session.shared_key.len() == 36 || session.shared_key.len() == 44);
}

#[test]
fn test_session_metadata_no_services() {
    let session = Session::new();
    let metadata = session.metadata();

    // Should have session UUID header
    assert!(metadata.contains_key("X-Docker-Expose-Session-Uuid"));
    assert_eq!(metadata.get("X-Docker-Expose-Session-Uuid").unwrap()[0], session.id);

    // Should have shared key header
    assert!(metadata.contains_key("X-Docker-Expose-Session-Sharedkey"));
    assert_eq!(metadata.get("X-Docker-Expose-Session-Sharedkey").unwrap()[0], session.shared_key);

    // Should have name header
    assert!(metadata.contains_key("X-Docker-Expose-Session-Name"));
}

#[test]
fn test_filesync_server_creation() {
    let temp_dir = std::env::temp_dir();
    let server = FileSyncServer::new(temp_dir.clone());

    assert_eq!(server.get_root_path(), temp_dir);
}

#[test]
fn test_filesync_server_with_different_paths() {
    let temp_dir1 = std::env::temp_dir().join("buildkit_test_1");
    let temp_dir2 = std::env::temp_dir().join("buildkit_test_2");

    std::fs::create_dir_all(&temp_dir1).unwrap();
    std::fs::create_dir_all(&temp_dir2).unwrap();

    let server1 = FileSyncServer::new(temp_dir1.clone());
    let server2 = FileSyncServer::new(temp_dir2.clone());

    assert_eq!(server1.get_root_path(), temp_dir1);
    assert_eq!(server2.get_root_path(), temp_dir2);

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir1);
    let _ = std::fs::remove_dir_all(&temp_dir2);
}

#[test]
fn test_auth_server_creation() {
    let _auth = AuthServer::new();
    // Successfully created auth server
}

#[test]
fn test_auth_server_with_registries() {
    let mut auth = AuthServer::new();

    auth.add_registry(RegistryAuthConfig {
        host: "docker.io".to_string(),
        username: "user1".to_string(),
        password: "pass1".to_string(),
    });

    auth.add_registry(RegistryAuthConfig {
        host: "gcr.io".to_string(),
        username: "user2".to_string(),
        password: "pass2".to_string(),
    });

    auth.add_registry(RegistryAuthConfig {
        host: "localhost:5000".to_string(),
        username: "admin".to_string(),
        password: "secret".to_string(),
    });

    // Successfully created auth server with multiple registries
}

#[test]
fn test_session_with_file_sync() {
    let temp_dir = std::env::temp_dir();
    let mut session = Session::new();

    session.add_file_sync(temp_dir.clone());

    let metadata = session.metadata();

    // Should expose gRPC methods
    let methods = metadata.get("X-Docker-Expose-Session-Grpc-Method");
    assert!(methods.is_some());

    let methods = methods.unwrap();
    assert!(methods.contains(&"/moby.filesync.v1.FileSync/DiffCopy".to_string()));
}

#[test]
fn test_session_with_auth() {
    let mut session = Session::new();
    session.add_auth(AuthServer::new());

    let metadata = session.metadata();

    // Should expose gRPC methods including Auth
    let methods = metadata.get("X-Docker-Expose-Session-Grpc-Method");
    assert!(methods.is_some());

    let methods = methods.unwrap();
    assert!(methods.contains(&"/moby.filesync.v1.Auth/Credentials".to_string()));
    assert!(methods.contains(&"/moby.filesync.v1.Auth/FetchToken".to_string()));
}

#[test]
fn test_session_metadata_format() {
    let mut session = Session::new();

    // Add services
    session.add_file_sync(std::env::temp_dir());
    session.add_auth(AuthServer::new());

    let metadata = session.metadata();

    // Verify UUID header exists and has correct format
    let uuid = metadata.get("X-Docker-Expose-Session-Uuid").unwrap();
    assert_eq!(uuid.len(), 1);
    assert_eq!(uuid[0], session.id);

    // Verify shared key header
    let key = metadata.get("X-Docker-Expose-Session-Sharedkey").unwrap();
    assert_eq!(uuid.len(), 1);
    assert_eq!(key[0], session.shared_key);

    // Verify name header
    let name = metadata.get("X-Docker-Expose-Session-Name");
    assert!(name.is_some());

    // Verify gRPC methods are exposed
    let methods = metadata.get("X-Docker-Expose-Session-Grpc-Method");
    assert!(methods.is_some());

    let methods = methods.unwrap();
    // Should have FileSync and Auth methods
    assert!(methods.contains(&"/moby.filesync.v1.FileSync/DiffCopy".to_string()));
    assert!(methods.contains(&"/moby.filesync.v1.Auth/Credentials".to_string()));
    assert!(methods.contains(&"/moby.filesync.v1.Auth/FetchToken".to_string()));
}

#[tokio::test]
async fn test_session_channel_creation() {
    let session = Session::new();

    // Creating a session should work without requiring a BuildKit connection
    // This is mainly a compilation and basic functionality test
    assert!(!session.id.is_empty());
}

#[test]
fn test_session_with_both_services() {
    let temp_dir = std::env::temp_dir();

    let mut session = Session::new();
    session.add_file_sync(temp_dir);
    session.add_auth(AuthServer::new());

    let metadata = session.metadata();

    // Should have gRPC methods
    let methods = metadata.get("X-Docker-Expose-Session-Grpc-Method");
    assert!(methods.is_some());

    let methods = methods.unwrap();
    // Should have both FileSync and Auth methods
    assert!(methods.contains(&"/moby.filesync.v1.FileSync/DiffCopy".to_string()));
    assert!(methods.contains(&"/moby.filesync.v1.Auth/Credentials".to_string()));
    assert!(methods.contains(&"/moby.filesync.v1.Auth/FetchToken".to_string()));
}

#[test]
fn test_multiple_session_instances() {
    let session1 = Session::new();
    let session2 = Session::new();
    let session3 = Session::new();

    // Each session should have unique IDs
    assert_ne!(session1.id, session2.id);
    assert_ne!(session2.id, session3.id);
    assert_ne!(session1.id, session3.id);

    // Each session should have unique shared keys
    assert_ne!(session1.shared_key, session2.shared_key);
    assert_ne!(session2.shared_key, session3.shared_key);
    assert_ne!(session1.shared_key, session3.shared_key);
}

#[test]
fn test_session_exposes_health_check() {
    let session = Session::new();
    let metadata = session.metadata();

    let methods = metadata.get("X-Docker-Expose-Session-Grpc-Method");
    assert!(methods.is_some());

    let methods = methods.unwrap();
    // Should always expose health check
    assert!(methods.contains(&"/grpc.health.v1.Health/Check".to_string()));
}
