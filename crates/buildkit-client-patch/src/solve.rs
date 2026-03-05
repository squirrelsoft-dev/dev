//! BuildKit solve operation implementation

use crate::builder::{BuildConfig, DockerfileSource};
use crate::client::BuildKitClient;
use crate::error::{Error, Result};
use crate::progress::ProgressHandler;
use crate::session::{Session, FileSync};
use crate::proto::moby::buildkit::v1::{
    Exporter, SolveRequest, StatusRequest, CacheOptions, CacheOptionsEntry,
};
use std::collections::HashMap;
use tokio_stream::StreamExt;
use uuid::Uuid;

/// Build result containing the image digest and metadata
#[derive(Debug)]
pub struct BuildResult {
    /// Container image digest
    pub digest: Option<String>,
    /// Export metadata
    pub metadata: HashMap<String, String>,
}

impl BuildKitClient {
    /// Execute a build operation with the given configuration
    ///
    /// # Arguments
    /// * `config` - Build configuration
    /// * `progress_handler` - Optional progress handler for real-time updates
    ///
    /// # Returns
    /// Build result containing digest and metadata
    pub async fn build(
        &mut self,
        config: BuildConfig,
        mut progress_handler: Option<Box<dyn ProgressHandler>>,
    ) -> Result<BuildResult> {
        // Generate unique build reference
        let build_ref = format!("build-{}", Uuid::new_v4());
        tracing::info!("Starting build with ref: {}", build_ref);

        // Create and start session
        let mut session = Session::new();

        // Add file sync for local builds
        if let DockerfileSource::Local { context_path, .. } = &config.source {
            let abs_path = std::fs::canonicalize(context_path)
                .map_err(|e| Error::PathResolution {
                    path: context_path.clone(),
                    source: e,
                })?;
            session.add_file_sync(abs_path).await;
        }

        // Add auth for registry authentication
        if let Some(ref registry_auth) = config.registry_auth {
            let mut auth = crate::session::AuthServer::new();
            auth.add_registry(crate::session::RegistryAuthConfig {
                host: registry_auth.host.clone(),
                username: registry_auth.username.clone(),
                password: registry_auth.password.clone(),
            });
            session.add_auth(auth).await;
        }

        // Add secrets if provided
        if !config.secrets.is_empty() {
            let secrets = crate::session::SecretsServer::from_map(config.secrets.clone())
                .map_err(|e| Error::secrets(format!("Failed to create secrets server: {}", e)))?;
            session.add_secrets(secrets).await;
            tracing::debug!("Added {} secrets to session", config.secrets.len());
        }

        // Start the session by connecting to BuildKit
        session.start(self.control().clone()).await?;

        tracing::info!("Session started: {}", session.get_id());

        // Prepare frontend attributes
        let mut frontend_attrs = HashMap::new();

        // Set dockerfile filename
        match &config.source {
            DockerfileSource::Local { dockerfile_path, .. } => {
                if let Some(path) = dockerfile_path {
                    frontend_attrs.insert(
                        "filename".to_string(),
                        path.to_string_lossy().to_string(),
                    );
                }
            }
            DockerfileSource::GitHub { dockerfile_path, .. } => {
                if let Some(path) = dockerfile_path {
                    frontend_attrs.insert("filename".to_string(), path.clone());
                }
            }
        }

        // Add build args
        for (key, value) in &config.build_args {
            frontend_attrs.insert(format!("build-arg:{}", key), value.clone());
        }

        // Set target stage
        if let Some(target) = &config.target {
            frontend_attrs.insert("target".to_string(), target.clone());
        }

        // Set platforms
        if !config.platforms.is_empty() {
            let platforms_str = config
                .platforms
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",");
            frontend_attrs.insert("platform".to_string(), platforms_str);
        }

        // Set no-cache
        if config.no_cache {
            frontend_attrs.insert("no-cache".to_string(), "true".to_string());
        }

        // Set pull
        if config.pull {
            frontend_attrs.insert("image-resolve-mode".to_string(), "pull".to_string());
        }

        // Prepare context source
        let context = self.prepare_context(&config, &session).await?;
        frontend_attrs.insert("context".to_string(), context);

        // Prepare exports (push to registry)
        let mut exports = Vec::new();
        if !config.tags.is_empty() {
            let mut export_attrs = HashMap::new();
            export_attrs.insert("name".to_string(), config.tags.join(","));
            export_attrs.insert("push".to_string(), "true".to_string());

            // Check if registry needs insecure flag based on tag or registry_auth
            let registry_host = if let Some(auth) = &config.registry_auth {
                Some(auth.host.as_str())
            } else {
                // Extract registry host from the first tag (format: host/image:tag or image:tag)
                config.tags.first().and_then(|tag| {
                    let parts: Vec<&str> = tag.split('/').collect();
                    if parts.len() > 1 && (parts[0].contains(':') || parts[0].contains('.') || parts[0] == "localhost") {
                        Some(parts[0])
                    } else {
                        None
                    }
                })
            };

            // Determine if registry is insecure (HTTP instead of HTTPS)
            if let Some(host) = registry_host {
                let is_insecure = host.starts_with("localhost")
                    || host.starts_with("127.0.0.1")
                    || host.starts_with("registry:") // Docker Compose service name
                    || (!host.contains('.') && !host.starts_with("docker.io")); // Simple heuristic for local names

                if is_insecure {
                    export_attrs.insert("registry.insecure".to_string(), "true".to_string());
                }
            }

            exports.push(Exporter {
                r#type: "image".to_string(),
                attrs: export_attrs,
            });
        }

        // Prepare cache imports
        let cache_imports = config
            .cache_from
            .iter()
            .map(|source| {
                let mut attrs = HashMap::new();
                attrs.insert("ref".to_string(), source.clone());
                CacheOptionsEntry {
                    r#type: "registry".to_string(),
                    attrs,
                }
            })
            .collect();

        // Prepare cache exports
        let cache_exports = config
            .cache_to
            .iter()
            .map(|dest| {
                let mut attrs = HashMap::new();
                attrs.insert("ref".to_string(), dest.clone());
                attrs.insert("mode".to_string(), "max".to_string());
                CacheOptionsEntry {
                    r#type: "registry".to_string(),
                    attrs,
                }
            })
            .collect();

        // Debug: Log exporter configuration
        tracing::debug!("Configured {} exporters", exports.len());
        for (i, exporter) in exports.iter().enumerate() {
            tracing::debug!("Exporter {}: type={}, attrs={:?}", i, exporter.r#type, exporter.attrs);
        }

        // Create solve request with session
        let request = SolveRequest {
            r#ref: build_ref.clone(),
            definition: None,
            exporter_deprecated: String::new(),
            exporter_attrs_deprecated: HashMap::new(),
            session: session.get_id(),  // Use session ID
            frontend: "dockerfile.v0".to_string(),
            frontend_attrs,
            cache: Some(CacheOptions {
                export_ref_deprecated: String::new(),
                import_refs_deprecated: vec![],
                export_attrs_deprecated: HashMap::new(),
                exports: cache_exports,
                imports: cache_imports,
            }),
            entitlements: vec![],
            frontend_inputs: HashMap::new(),
            internal: false,
            source_policy: None,
            exporters: exports,
            enable_session_exporter: false,
            source_policy_session: String::new(),
        };

        // Start the build
        tracing::info!("Sending solve request to buildkit");

        // Create request with session metadata headers
        let mut grpc_request = tonic::Request::new(request);
        let metadata = grpc_request.metadata_mut();

        // Add session metadata headers
        for (key, values) in session.metadata() {
            if let Ok(k) = key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>() {
                // Add each value for the key (supports multi-value headers)
                for value in values {
                    if let Ok(v) = value.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                        metadata.append(k.clone(), v);
                    }
                }
            }
        }

        let response = self
            .control()
            .solve(grpc_request)
            .await?;

        let solve_response = response.into_inner();

        // Monitor build progress if handler is provided
        if let Some(ref mut handler) = progress_handler {
            self.monitor_progress(&build_ref, handler).await?;
        }

        // Extract digest and metadata
        let digest = solve_response
            .exporter_response
            .get("containerimage.digest")
            .cloned();

        tracing::info!("Build completed successfully");
        if let Some(ref d) = digest {
            tracing::info!("Image digest: {}", d);
        }

        Ok(BuildResult {
            digest,
            metadata: solve_response.exporter_response,
        })
    }

    /// Prepare build context based on source type
    async fn prepare_context(&self, config: &BuildConfig, session: &Session) -> Result<String> {
        match &config.source {
            DockerfileSource::Local { context_path, .. } => {
                // Validate the context path
                let file_sync = FileSync::new(context_path);
                file_sync.validate()?;

                // Use session-based input
                // The format is: input:<name> where name references the session
                Ok(format!("input:{}:context", session.shared_key))
            }
            DockerfileSource::GitHub {
                repo_url,
                git_ref,
                token,
                ..
            } => {
                let mut url = repo_url.clone();

                // Ensure URL ends with .git for Git protocol
                if !url.ends_with(".git") {
                    url.push_str(".git");
                }

                // Add authentication token if provided
                if let Some(token) = token {
                    // Format: https://token@github.com/user/repo.git
                    url = url.replace("https://", &format!("https://{}@", token));
                }

                // Add git reference
                if let Some(git_ref) = git_ref {
                    url = format!("{}#{}", url, git_ref);
                }

                Ok(url)
            }
        }
    }

    /// Monitor build progress and send updates to the handler
    async fn monitor_progress(
        &mut self,
        build_ref: &str,
        handler: &mut Box<dyn ProgressHandler>,
    ) -> Result<()> {
        let status_request = StatusRequest {
            r#ref: build_ref.to_string(),
        };

        let mut stream = self
            .control()
            .status(status_request)
            .await?
            .into_inner();

        handler.on_start()?;

        while let Some(response) = stream.next().await {
            match response {
                Ok(status) => {
                    handler.on_status(status)?;
                }
                Err(e) => {
                    tracing::error!("Status stream error: {}", e);
                    handler.on_error(&e.to_string())?;
                    break;
                }
            }
        }

        handler.on_complete()?;
        Ok(())
    }
}
