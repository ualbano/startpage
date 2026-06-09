use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use clap::{Parser, Subcommand};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "docker-image-checker")]
#[command(about = "Checks for new Docker images and pulls them automatically")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Path to state file (stores known digests)
    #[arg(short, long, default_value = "state.json")]
    state: PathBuf,

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
struct Config {
    images: Vec<ImageConfig>,
}

#[derive(Deserialize)]
struct ImageConfig {
    name: String,
    #[serde(default = "default_tag")]
    tag: String,
}

fn default_tag() -> String {
    "latest".to_string()
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    digests: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

struct DockerChecker {
    client: Client,
    state_path: PathBuf,
    state: State,
}

impl DockerChecker {
    fn new(state_path: PathBuf) -> anyhow::Result<Self> {
        let state = if state_path.exists() {
            let content = fs::read_to_string(&state_path)?;
            serde_json::from_str(&content)?
        } else {
            State::default()
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            state_path,
            state,
        })
    }

    async fn get_token(&self, image: &str) -> anyhow::Result<String> {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            image
        );
        let resp: TokenResponse = self.client.get(&url).send().await?.json().await?;
        Ok(resp.token)
    }

    async fn get_remote_digest(&self, image: &str, tag: &str) -> anyhow::Result<String> {
        // Official images live under library/ in the registry
        let full_image = if image.contains('/') {
            image.to_string()
        } else {
            format!("library/{}", image)
        };

        let token = self.get_token(&full_image).await?;

        let url = format!(
            "https://registry-1.docker.io/v2/{}/manifests/{}",
            full_image, tag
        );

        let response = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Registry returned HTTP {}", status);
        }

        let digest = response
            .headers()
            .get("Docker-Content-Digest")
            .ok_or_else(|| anyhow::anyhow!("Missing Docker-Content-Digest header"))?
            .to_str()?
            .to_string();

        Ok(digest)
    }

    async fn check_and_pull(&mut self, image: &str, tag: &str) -> anyhow::Result<bool> {
        let key = format!("{}:{}", image, tag);
        print!("Checking {:40} ", key);

        let remote_digest = match self.get_remote_digest(image, tag).await {
            Ok(d) => d,
            Err(e) => {
                println!("[ERROR: {}]", e);
                return Ok(false);
            }
        };

        let known = self.state.digests.get(&key);
        let short_digest = &remote_digest[7..19]; // skip "sha256:" prefix

        if known.map_or(true, |d| d != &remote_digest) {
            println!("[NEW {}]", short_digest);

            let status = Command::new("docker").args(["pull", &key]).status()?;

            if status.success() {
                println!("  -> Pulled successfully.");
                self.state.digests.insert(key, remote_digest);
                self.save_state()?;
                return Ok(true);
            } else {
                eprintln!("  -> docker pull failed (exit code: {:?})", status.code());
            }
        } else {
            println!("[up-to-date {}]", short_digest);
        }

        Ok(false)
    }

    fn save_state(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(&self.state)?;
        fs::write(&self.state_path, json)?;
        Ok(())
    }
}

async fn run_check(checker: &mut DockerChecker, images: &[ImageConfig]) -> anyhow::Result<()> {
    let mut updated = 0usize;
    for img in images {
        if checker.check_and_pull(&img.name, &img.tag).await? {
            updated += 1;
        }
    }
    println!("\n{} of {} image(s) updated.", updated, images.len());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config_content = fs::read_to_string(&cli.config)
        .map_err(|_| anyhow::anyhow!("Config file not found: {}", cli.config.display()))?;
    let config: Config = toml::from_str(&config_content)?;

    if config.images.is_empty() {
        anyhow::bail!("No images configured in {}", cli.config.display());
    }

    let mut checker = DockerChecker::new(cli.state)?;

    match cli.command {
        Commands::Check => {
            run_check(&mut checker, &config.images).await?;
        }
        Commands::Watch { interval } => {
            println!(
                "Watching {} image(s), checking every {}s. Press Ctrl+C to stop.\n",
                config.images.len(),
                interval
            );
            loop {
                run_check(&mut checker, &config.images).await?;
                println!("Next check in {}s.\n", interval);
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }

    Ok(())
}
