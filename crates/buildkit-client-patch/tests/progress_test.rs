//! Unit tests for progress handlers

use buildkit_client::progress::{ProgressHandler, ConsoleProgressHandler, JsonProgressHandler};
use buildkit_client::proto::moby::buildkit::v1::StatusResponse;

#[test]
fn test_console_progress_handler_creation() {
    let _handler = ConsoleProgressHandler::new(false);
    let _handler_verbose = ConsoleProgressHandler::new(true);
    // Should create without errors
}

#[test]
fn test_json_progress_handler_creation() {
    let _handler = JsonProgressHandler::new();
    // Should create without errors
}

#[test]
fn test_console_progress_handler_on_start() {
    let mut handler = ConsoleProgressHandler::new(false);
    let result = handler.on_start();
    assert!(result.is_ok());
}

#[test]
fn test_console_progress_handler_on_complete() {
    let mut handler = ConsoleProgressHandler::new(false);
    let result = handler.on_complete();
    assert!(result.is_ok());
}

#[test]
fn test_console_progress_handler_on_error() {
    let mut handler = ConsoleProgressHandler::new(false);
    let result = handler.on_error("test error");
    assert!(result.is_ok());
}

#[test]
fn test_console_progress_handler_on_status() {
    let mut handler = ConsoleProgressHandler::new(false);

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };

    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_json_progress_handler_on_status() {
    let mut handler = JsonProgressHandler::new();

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };

    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_console_progress_handler_verbose_mode() {
    let mut handler_quiet = ConsoleProgressHandler::new(false);
    let mut handler_verbose = ConsoleProgressHandler::new(true);

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };

    assert!(handler_quiet.on_status(status.clone()).is_ok());
    assert!(handler_verbose.on_status(status).is_ok());
}

#[test]
fn test_progress_handler_with_vertexes() {
    use buildkit_client::proto::moby::buildkit::v1::Vertex;
    use prost_types::Timestamp;

    let mut handler = ConsoleProgressHandler::new(false);

    let vertex = Vertex {
        digest: "sha256:abc123".to_string(),
        inputs: vec![],
        name: "Test vertex".to_string(),
        cached: false,
        started: Some(Timestamp { seconds: 0, nanos: 0 }),
        completed: None,
        error: String::new(),
        progress_group: None,
    };

    let status = StatusResponse {
        vertexes: vec![vertex],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };

    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_progress_handler_with_logs() {
    use buildkit_client::proto::moby::buildkit::v1::VertexLog;
    use prost_types::Timestamp;

    let mut handler = ConsoleProgressHandler::new(true);

    let log = VertexLog {
        vertex: "sha256:abc123".to_string(),
        timestamp: Some(Timestamp { seconds: 0, nanos: 0 }),
        stream: 1, // stdout
        msg: b"Test log message".to_vec(),
    };

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![log],
        warnings: vec![],
    };

    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_progress_handler_with_warnings() {
    use buildkit_client::proto::moby::buildkit::v1::VertexWarning;

    let mut handler = ConsoleProgressHandler::new(false);

    let warning = VertexWarning {
        vertex: "sha256:abc123".to_string(),
        level: 1,
        short: b"Warning".to_vec(),
        detail: vec![b"Warning details".to_vec()],
        url: String::new(),
        info: None,
        ranges: vec![],
    };

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![warning],
    };

    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_json_progress_output_format() {
    use buildkit_client::proto::moby::buildkit::v1::Vertex;
    use prost_types::Timestamp;

    let mut handler = JsonProgressHandler::new();

    let vertex = Vertex {
        digest: "sha256:abc123".to_string(),
        inputs: vec![],
        name: "Test operation".to_string(),
        cached: false,
        started: Some(Timestamp { seconds: 1234567890, nanos: 0 }),
        completed: Some(Timestamp { seconds: 1234567900, nanos: 0 }),
        error: String::new(),
        progress_group: None,
    };

    let status = StatusResponse {
        vertexes: vec![vertex],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };

    // JSON handler should be able to serialize the status
    let result = handler.on_status(status);
    assert!(result.is_ok());
}

#[test]
fn test_json_handler_lifecycle() {
    let mut handler = JsonProgressHandler::new();

    assert!(handler.on_start().is_ok());

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };
    assert!(handler.on_status(status).is_ok());

    assert!(handler.on_complete().is_ok());
}

#[test]
fn test_console_handler_lifecycle() {
    let mut handler = ConsoleProgressHandler::new(false);

    assert!(handler.on_start().is_ok());

    let status = StatusResponse {
        vertexes: vec![],
        statuses: vec![],
        logs: vec![],
        warnings: vec![],
    };
    assert!(handler.on_status(status).is_ok());

    assert!(handler.on_complete().is_ok());
}
