mod stream;

use anyhow::{Context, Result};
use futures::{pin_mut, Stream, StreamExt as _};
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use http::Response;
use indicatif::ProgressStyle;
use octocrab::models::commits::Commit;
use octocrab::models::{Rate, Repository};
use octocrab::{FromResponse, Octocrab, Page};
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snafu::GenerateImplicitData;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use tracing::{info, instrument, Instrument, Span};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use url::Url;

type DefaultRateLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

static BANNED_REPOS: Lazy<HashSet<String>> = Lazy::new(|| {
    HashSet::from(
        [
            "rustdesk/rustdesk",                           // proprietary stuff
            "rustdesk/rustdesk-server",                    // proprietary stuff
            "rust-lang/rustlings",                         // teaching project
            "rust-unofficial/awesome-rust",                // not actually rust code
            "lencx/ChatGPT",                               // AI
            "FuelLabs/sway",                               // crypto
            "FuelLabs/fuel-core",                          // crypto
            "google/comprehensive-rust",                   // teaching project
            "sunface/rust-course",                         // teaching project
            "copy/v86",                                    // not only rust
            "facebook/relay",                              // this is a JS framework
            "TheAlgorithms/Rust",                          // teaching project
            "diem/diem",                                   // crypto
            "FuelLabs/fuels-rs",                           // crypto
            "rust-lang/book",                              // this is docs
            "analysis-tools-dev/static-analysis",          // not actually rust code
            "solana-labs/solana",                          // crypto
            "pingcap/talent-plan",                         // teaching project
            "jauhien/iron-kaleidoscope",                   // teaching project
            "sunface/rust-by-practice",                    // teaching project
            "paritytech/polkadot",                         // crypto
            "openethereum/parity-ethereum",                // crypto
            "Reamd7/notion-zh_CN",                         // not only rust code
            "pretzelhammer/rust-blog",                     // not code
            "aptos-labs/aptos-core",                       // crypto
            "MystenLabs/sui",                              // crypto
            "facebook/sapling",                            // not only rust code
            "utilForever/game-developer-roadmap",          // not code
            "mimblewimble/grin",                           // crypto
            "crablang/crab",                               // just a mirror ("fork") of rust
            "rust-embedded/rust-raspberrypi-OS-tutorials", // teaching project
            "flosse/rust-web-framework-comparison",        // not code
            // iteration 2
            "headwindsim/A339X",                   // not majority rust
            "EvilGenius-dot/RustMinerSystem",      // crypto, a collection of blobs
            "a137x/plutus-rustus",                 // not majority rust
            "NodleCode/chain",                     // crypto stuff
            "parity-asia/hackathon-2021-spring",   // teaching project
            "DaZiYuan/livewallpaper",              // blobs
            "dfinity/internet-identity",           // crypto
            "paritytech/polkadot-sdk",             // crypto
            "reison1218/GameServer_Rust",          // blobs
            "LayerXcom/zero-chain",                // crypto
            "ravenclaw900/DietPi-Dashboard",       // blobs, not majority rust
            "tyrchen/rust-training",               // teaching project
            "parity-asia/hackathon-2023-summer",   // teaching project
            "second-state/wasm-learning",          // teaching project
            "standardweb3/standard-substrate",     // crypto
            "linera-io/linera-protocol",           // crypto
            "mkb2091/blockconvert",                // blobs
            "HorizenOfficial/zendoo-sc-cryptolib", // crypto
            "MadrigalStreetCartel/neuz",           // blobs
            "dashpay/platform",                    // crypto
        ]
        .map(|s| s.to_string()),
    )
});

const REPO_COUNT: usize = 1000;

fn pre_filter_repo(repo: &Repository) -> bool {
    let repo_name = repo.full_name.as_deref().unwrap_or_default();

    if repo.archived.unwrap_or_default() {
        info!("Pre-skip: Archived: {}", repo_name);
        return false;
    }

    if BANNED_REPOS.contains(repo_name) {
        info!("Pre-skip: Banned: {}", repo_name);
        return false;
    }

    true
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TreeItemType {
    Tree,
    Blob,
    Commit,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TreeItem {
    pub path: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub type_: TreeItemType,
    pub sha: String,
    pub url: Option<Url>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    pub sha: String,
    pub url: Url,
    pub tree: Vec<TreeItem>,
    pub truncated: bool,
}

impl Tree {
    pub fn total_size(&self) -> u64 {
        self.tree
            .iter()
            .map(|item| item.size.unwrap_or_default())
            .sum()
    }
    pub fn rust_size(&self) -> u64 {
        self.tree
            .iter()
            .filter(|item| item.path.ends_with(".rs"))
            .map(|item| item.size.unwrap_or_default())
            .sum()
    }
    pub fn rust_percentage(&self) -> f64 {
        self.rust_size() as f64 / self.total_size() as f64
    }
}

struct RateLimitInfo<T: DeserializeOwned> {
    content: T,
    limit: u32,
    used: u32,
    remaining: u32,
    resource: String,
}

#[async_trait::async_trait]
impl<T: DeserializeOwned> FromResponse for RateLimitInfo<T> {
    async fn from_response(response: Response<hyper::Body>) -> octocrab::Result<Self> {
        let headers = response.headers();
        let limit = headers
            .get("x-ratelimit-limit")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let used = headers
            .get("x-ratelimit-used")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let remaining = headers
            .get("x-ratelimit-remaining")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let resource = headers
            .get("x-ratelimit-resource")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let json: serde_json::Value = serde_json::from_slice(
            hyper::body::to_bytes(response.into_body())
                .await
                .map_err(|source| octocrab::Error::Hyper {
                    source,
                    backtrace: snafu::Backtrace::generate(),
                })?
                .as_ref(),
        )
        .map_err(|source| octocrab::Error::Serde {
            source,
            backtrace: snafu::Backtrace::generate(),
        })?;

        Ok(Self {
            content: serde_json::from_value(json).map_err(|source| octocrab::Error::Serde {
                source,
                backtrace: snafu::Backtrace::generate(),
            })?,
            limit,
            used,
            remaining,
            resource,
        })
    }
}

#[instrument(skip_all, fields(repo = %repo.full_name.as_deref().unwrap_or_default()))]
async fn filter_repo(
    crab: &Octocrab,
    repo: &Repository,
    rate_limiter: &DefaultRateLimiter,
) -> Result<bool> {
    let repo_name = repo.full_name.as_deref().unwrap_or_default();

    rate_limiter
        .until_ready()
        .instrument(tracing::info_span!("wait_rate_limit"))
        .await;
    let page: Page<Commit> = crab
        .get(
            format!("/repos/{repo_name}/commits?per_page=1",),
            None::<&()>,
        )
        .await
        .context("Getting commits")?;
    let last_page_url = Url::parse(&page.last.unwrap().to_string()).unwrap();
    let (_, commits) = last_page_url
        .query_pairs()
        .find(|(k, _)| k == "page")
        .unwrap();
    let commits = commits.parse::<u32>().unwrap();

    if commits < 10 {
        info!("Skip: Commits: {}", repo_name);
    }

    let latest_commit_sha = &page.items[0].sha;

    rate_limiter
        .until_ready()
        .instrument(tracing::info_span!("wait_rate_limit"))
        .await;
    let tree: RateLimitInfo<Tree> = crab
        .get(
            format!("/repos/{repo_name}/git/trees/{latest_commit_sha}?recursive=true"),
            None::<&()>,
        )
        .instrument(tracing::info_span!("get_tree"))
        .await
        .context("Getting the tree")?;
    if let Some(used) = NonZeroU32::new(tree.used - 1) {
        rate_limiter
            .until_n_ready(used)
            .instrument(tracing::info_span!("wait_rate_limit"))
            .await
            .context("Waiting for rate limit")?;
    }

    if tree.content.rust_percentage() < 0.51 {
        info!("Skip: Rust percentage: {}", repo_name);
        return Ok(false);
    }

    Ok(true)
}

// async fn clone_repo() -> Result<()> {}

fn progressbar_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("{span_child_prefix}{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta} @ {per_sec})")
        .unwrap()
        .progress_chars("#>-")
}

#[instrument(skip_all)]
async fn get_repo_list(
    crab: &Octocrab,
    repos: impl Stream<Item = Result<Repository>>,
    core_rate_limiter: &DefaultRateLimiter,
) -> Result<Vec<String>> {
    let span = Span::current();
    span.pb_set_style(&progressbar_style());
    span.pb_set_length(REPO_COUNT as u64);

    let mut repo_list = Vec::new();

    pin_mut!(repos);
    while let Some(repo) = repos.next().await {
        let repo = repo.context("Getting next repo")?;

        if !pre_filter_repo(&repo) {
            continue;
        }

        if !filter_repo(crab, &repo, core_rate_limiter)
            .await
            .context("Filtering repo")?
        {
            continue;
        }

        let repo_name = repo.full_name.as_deref().unwrap();
        info!("Pass: {}", repo_name);

        repo_list.push(repo_name.to_string());
        span.pb_set_position(repo_list.len() as u64);
        if repo_list.len() >= REPO_COUNT {
            break;
        }
    }

    Ok(repo_list)
}

async fn make_rate_limiter(rate: Rate) -> Result<DefaultRateLimiter> {
    // assuming all rates are reset every minute
    let quota = Quota::per_minute(NonZeroU32::new(rate.limit as u32).unwrap());
    let rate_limiter = RateLimiter::direct(quota);
    // preload
    if let Some(used) = NonZeroU32::new(rate.remaining as u32) {
        rate_limiter.until_n_ready(used).await.unwrap();
    }
    Ok(rate_limiter)
}

async fn main_impl() -> Result<()> {
    let token = std::env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN env variable is required");

    let crab = Octocrab::builder()
        .personal_token(token)
        .build()
        .context("Building octocrab")?;

    let rate_limit = crab
        .ratelimit()
        .get()
        .await
        .context("Getting the rate limit")?;
    info!("Search rate limits: {:?}", rate_limit.resources.search);

    let core_rate_limiter = make_rate_limiter(rate_limit.resources.core).await?;
    let search_rate_limiter = Arc::new(make_rate_limiter(rate_limit.resources.search).await?);

    let repos = crab
        .search()
        .repositories("stars:>10 forks:>10 language:Rust")
        .sort("stars")
        .order("desc")
        .send()
        .await
        .context("Getting repos")?;
    let repos = stream::into_stream(repos, &crab, search_rate_limiter.clone());

    let repos = get_repo_list(&crab, repos, &core_rate_limiter).await?;

    std::fs::write("repo-list.txt", repos.join("\n")).context("Writing repo list")?;

    Ok(())
}

const DEFAULT_ENV_FILTER: &str = "info";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    #[cfg(windows)]
    let _enabled = ansi_term::enable_ansi_support();

    let indicatif_layer = IndicatifLayer::new();
    let stderr = indicatif_layer.get_stderr_writer();

    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_ENV_FILTER))
        .with_subscriber(
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_writer(stderr))
                .with(indicatif_layer),
        )
        .init();

    main_impl().await
}
