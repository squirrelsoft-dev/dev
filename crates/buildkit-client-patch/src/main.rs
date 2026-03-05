use anyhow::Result;
use buildkit_client::{BuildConfig, BuildKitClient, Platform, RegistryAuth};
use buildkit_client::progress::{ConsoleProgressHandler, JsonProgressHandler};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "buildkit-client")]
#[command(about = "BuildKit Rust client for building container images", long_about = None)]
struct Cli {
    /// BuildKit daemon address
    #[arg(short, long, default_value = "http://localhost:1234")]
    addr: String,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build from a local Dockerfile
    Local {
        /// Context directory
        #[arg(short, long, default_value = ".")]
        context: PathBuf,

        /// Dockerfile path (relative to context or absolute)
        #[arg(short = 'f', long)]
        dockerfile: Option<PathBuf>,

        /// Image tags
        #[arg(short, long)]
        tag: Vec<String>,

        /// Build arguments
        #[arg(long)]
        build_arg: Vec<String>,

        /// Target stage
        #[arg(long)]
        target: Option<String>,

        /// Target platform (e.g., linux/amd64)
        #[arg(long)]
        platform: Vec<String>,

        /// Registry host for authentication
        #[arg(long)]
        registry_host: Option<String>,

        /// Registry username
        #[arg(long)]
        registry_user: Option<String>,

        /// Registry password
        #[arg(long)]
        registry_password: Option<String>,

        /// No cache
        #[arg(long)]
        no_cache: bool,

        /// Always pull base images
        #[arg(long)]
        pull: bool,

        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// Build from a GitHub repository
    Github {
        /// Repository URL
        repo: String,

        /// Git reference (branch, tag, or commit)
        #[arg(short = 'b', long)]
        git_ref: Option<String>,

        /// GitHub token for private repositories
        #[arg(long, env = "GITHUB_TOKEN")]
        token: Option<String>,

        /// Dockerfile path within the repository
        #[arg(short = 'f', long)]
        dockerfile: Option<String>,

        /// Image tags
        #[arg(short, long)]
        tag: Vec<String>,

        /// Build arguments
        #[arg(long)]
        build_arg: Vec<String>,

        /// Target stage
        #[arg(long)]
        target: Option<String>,

        /// Target platform (e.g., linux/amd64)
        #[arg(long)]
        platform: Vec<String>,

        /// Registry host for authentication
        #[arg(long)]
        registry_host: Option<String>,

        /// Registry username
        #[arg(long)]
        registry_user: Option<String>,

        /// Registry password
        #[arg(long)]
        registry_password: Option<String>,

        /// No cache
        #[arg(long)]
        no_cache: bool,

        /// Always pull base images
        #[arg(long)]
        pull: bool,

        /// JSON output
        #[arg(long)]
        json: bool,
    },

    /// Check BuildKit health
    Health,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();

    // Connect to BuildKit
    let mut client = BuildKitClient::connect(&cli.addr).await?;

    match cli.command {
        Commands::Local {
            context,
            dockerfile,
            tag,
            build_arg,
            target,
            platform,
            registry_host,
            registry_user,
            registry_password,
            no_cache,
            pull,
            json,
        } => {
            let mut config = BuildConfig::local(context);

            if let Some(df) = dockerfile {
                config = config.dockerfile(df.to_string_lossy().to_string());
            }

            for t in tag {
                config = config.tag(t);
            }

            for arg in build_arg {
                if let Some((key, value)) = arg.split_once('=') {
                    config = config.build_arg(key, value);
                }
            }

            if let Some(t) = target {
                config = config.target(t);
            }

            if !platform.is_empty() {
                config.platforms.clear();
                for p in platform {
                    config = config.platform(Platform::parse(&p)?);
                }
            }

            if let (Some(host), Some(user), Some(pass)) =
                (registry_host, registry_user, registry_password)
            {
                config = config.registry_auth(RegistryAuth {
                    host,
                    username: user,
                    password: pass,
                });
            }

            config = config.no_cache(no_cache).pull(pull);

            let progress: Box<dyn buildkit_client::progress::ProgressHandler> = if json {
                Box::new(JsonProgressHandler::new())
            } else {
                Box::new(ConsoleProgressHandler::new(cli.verbose))
            };

            let result = client.build(config, Some(progress)).await?;

            if let Some(digest) = result.digest {
                println!("\n📦 Image digest: {}", digest);
            }
        }

        Commands::Github {
            repo,
            git_ref,
            token,
            dockerfile,
            tag,
            build_arg,
            target,
            platform,
            registry_host,
            registry_user,
            registry_password,
            no_cache,
            pull,
            json,
        } => {
            let mut config = BuildConfig::github(repo);

            if let Some(git_ref) = git_ref {
                config = config.git_ref(git_ref);
            }

            if let Some(token) = token {
                config = config.github_token(token);
            }

            if let Some(df) = dockerfile {
                config = config.dockerfile(df);
            }

            for t in tag {
                config = config.tag(t);
            }

            for arg in build_arg {
                if let Some((key, value)) = arg.split_once('=') {
                    config = config.build_arg(key, value);
                }
            }

            if let Some(t) = target {
                config = config.target(t);
            }

            if !platform.is_empty() {
                config.platforms.clear();
                for p in platform {
                    config = config.platform(Platform::parse(&p)?);
                }
            }

            if let (Some(host), Some(user), Some(pass)) =
                (registry_host, registry_user, registry_password)
            {
                config = config.registry_auth(RegistryAuth {
                    host,
                    username: user,
                    password: pass,
                });
            }

            config = config.no_cache(no_cache).pull(pull);

            let progress: Box<dyn buildkit_client::progress::ProgressHandler> = if json {
                Box::new(JsonProgressHandler::new())
            } else {
                Box::new(ConsoleProgressHandler::new(cli.verbose))
            };

            let result = client.build(config, Some(progress)).await?;

            if let Some(digest) = result.digest {
                println!("\n📦 Image digest: {}", digest);
            }
        }

        Commands::Health => {
            client.health_check().await?;
            println!("✅ BuildKit is healthy");
        }
    }

    Ok(())
}
