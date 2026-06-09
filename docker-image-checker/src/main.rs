use std::collections::HashSet;
use std::time::Duration;

use bollard::container::ListContainersOptions;
use bollard::image::CreateImageOptions;
use bollard::Docker;
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "docker-image-checker")]
#[command(about = "Finds running containers, checks for image updates, and pulls them")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check once for updates and pull if needed
    Check,
    /// Watch continuously and pull on updates
    Watch {
        /// Check interval in seconds
        #[arg(short, long, default_value = "300")]
        interval: u64,
    },
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

struct Checker {
    docker: Docker,
    http: Client,
}

impl Checker {
    fn new() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { docker, http })
    }

    /// Returns the unique set of image references used by running containers.
    /// Images pinned by digest (foo@sha256:…) are skipped — they can't be updated by tag.
    async fn running_images(&self) -> anyhow::Result<Vec<String>> {
        let opts: ListContainersOptions<String> = ListContainersOptions {
            all: false, // running only
            ..Default::default()
        };
        let containers = self.docker.list_containers(Some(opts)).await?;

        let mut seen: HashSet<String> = HashSet::new();
        for c in containers {
            if let Some(img) = c.image {
                if !img.contains('@') {
                    seen.insert(img);
                }
            }
        }

        let mut images: Vec<String> = seen.into_iter().collect();
        images.sort();
        Ok(images)
    }

    /// Returns the content-digest of the locally stored image, if available.
    /// Docker stores the pull-digest in RepoDigests as "name@sha256:…".
    async fn local_digest(&self, image_ref: &str) -> Option<String> {
        let info = self.docker.inspect_image(image_ref).await.ok()?;
        info.repo_digests?
            .into_iter()
            .next()
            .and_then(|s| s.split('@').nth(1).map(str::to_owned))
    }

    async fn registry_token(&self, repo: &str) -> anyhow::Result<String> {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            repo
        );
        let resp: TokenResponse = self.http.get(&url).send().await?.json().await?;
        Ok(resp.token)
    }

    /// Queries Docker Hub for the current content-digest of `repo:tag`.
    async fn remote_digest(&self, repo: &str, tag: &str) -> anyhow::Result<String> {
        // Official images live under "library/" in the registry API
        let full_repo = if repo.contains('/') {
            repo.to_string()
        } else {
            format!("library/{}", repo)
        };

        let token = self.registry_token(&full_repo).await?;
        let url = format!(
            "https://registry-1.docker.io/v2/{}/manifests/{}",
            full_repo, tag
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Registry returned HTTP {}", resp.status());
        }

        resp.headers()
            .get("Docker-Content-Digest")
            .ok_or_else(|| anyhow::anyhow!("Missing Docker-Content-Digest header"))?
            .to_str()
            .map(str::to_owned)
            .map_err(Into::into)
    }

    /// Pulls an image via the Docker daemon and streams progress to stdout.
    async fn pull(&self, repo: &str, tag: &str) -> anyhow::Result<()> {
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: repo,
                tag,
                ..Default::default()
            }),
            None,
            None,
        );

        while let Some(item) = stream.next().await {
            let info = item?;
            if let (Some(status), Some(progress)) = (info.status, info.progress) {
                print!("\r    {}: {:<50}", status, progress);
            }
        }
        // Clear the progress line
        print!("\r{:<80}\r", "");

        Ok(())
    }

    async fn check_image(&self, image_ref: &str) -> anyhow::Result<bool> {
        let (repo, tag) = image_ref
            .split_once(':')
            .unwrap_or((image_ref, "latest"));

        print!("  {:<48} ", image_ref);

        let remote = match self.remote_digest(repo, tag).await {
            Ok(d) => d,
            Err(e) => {
                println!("[ERROR] {}", e);
                return Ok(false);
            }
        };

        let local = self.local_digest(image_ref).await;
        let short = &remote[7..19]; // "sha256:" is 7 chars

        if local.as_deref() == Some(remote.as_str()) {
            println!("[up-to-date {}]", short);
            return Ok(false);
        }

        println!("[NEW {}]", short);
        print!("    Pulling...");

        match self.pull(repo, tag).await {
            Ok(()) => {
                println!("    Done.");
                Ok(true)
            }
            Err(e) => {
                eprintln!("    Pull failed: {}", e);
                Ok(false)
            }
        }
    }

    async fn run_check(&self) -> anyhow::Result<()> {
        let images = self.running_images().await?;

        if images.is_empty() {
            println!("No running containers found.");
            return Ok(());
        }

        println!("Running containers use {} unique image(s):", images.len());

        let mut updated = 0usize;
        for img in &images {
            if self.check_image(img).await? {
                updated += 1;
            }
        }

        println!("\n{} of {} image(s) updated.", updated, images.len());
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let checker = Checker::new().map_err(|e| {
        anyhow::anyhow!(
            "Cannot connect to Docker daemon (is it running?): {}",
            e
        )
    })?;

    match cli.command {
        Commands::Check => {
            checker.run_check().await?;
        }
        Commands::Watch { interval } => {
            println!("Watching running containers every {}s. Press Ctrl+C to stop.\n", interval);
            loop {
                checker.run_check().await?;
                println!("Next check in {}s.\n", interval);
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }

    Ok(())
}
