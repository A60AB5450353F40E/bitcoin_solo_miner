mod addresses;
use addresses::{advance_after_block, init_addresses, spawn_file_watcher};

mod block_template;

mod block_updater;
use block_updater::update_block;

mod merkle;
mod mine;
use mine::mine_loop;

mod utils;
use utils::{
    MIN_DIFFICULTY_RULE, MinerResult, RpcTimeout, SubmitOutcome, TARGET_TIME, TimestampMode,
    build_full_block, load_sentences_from_file, min_difficulty_boundary, preflight, submit_block,
};

use dotenvy::var;
use once_cell::sync::Lazy;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, RwLock};

// Re-exported for block_updater's `use crate::fetch_block_template;`.
pub use block_template::{BlockTemplate, fetch_block_template};

pub static SENTENCES: Lazy<std::sync::RwLock<Vec<String>>> =
    Lazy::new(|| std::sync::RwLock::new(Vec::new()));

/// (template, fingerprint of that template). Workers compare the fingerprint
/// against the cached transaction set before mining.
pub type LiveTemplate = (Arc<BlockTemplate>, u64);

pub static GLOBAL_TEMPLATE: Lazy<RwLock<Option<LiveTemplate>>> = Lazy::new(|| RwLock::new(None));

/// The address the next coinbase pays. `None` means "do not mine" (nothing
/// selected yet, or a `once` list that has been used up).
pub static PAYOUT_ADDRESS: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));

/// Signalled after a submitted block so the updater drops its longpoll and
/// fetches the next template immediately.
pub static REFRESH: Lazy<Notify> = Lazy::new(Notify::new);

fn load_dotenv() -> MinerResult<PathBuf> {
    if let Ok(explicit) = var("DOTENV_PATH") {
        let path = PathBuf::from(explicit);
        if !path.exists() {
            return Err(format!("DOTENV_PATH points at missing file {}", path.display()).into());
        }
        dotenvy::from_path(&path)?;
        return Ok(path);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".env"));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
    {
        candidates.push(dir.join(".env"));
    }

    let existing: Vec<PathBuf> = candidates.into_iter().filter(|p| p.exists()).collect();
    if existing.len() > 1 {
        eprintln!(
            "[env] WARNING: multiple .env files present: {:?} — using {}",
            existing,
            existing[0].display()
        );
    }

    let path = existing.into_iter().next().ok_or_else(|| {
        format!(
            "no .env in {} and none beside the binary",
            std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "?".into())
        )
    })?;

    dotenvy::from_path(&path)?;
    Ok(path)
}

fn clean(value: String) -> String {
    value.trim_end_matches(['\r', '\n']).to_string()
}

fn required(key: &str) -> MinerResult<String> {
    var(key).map_err(|_| format!("{key} missing from environment/.env").into())
}

/// Workers are CPU-bound, so running more of them than the CPUs this process
/// can actually use only adds scheduler churn. `available_parallelism` honours
/// CPU affinity and cgroup quotas, unlike a host core count.
fn resolve_num_tasks(requested: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if requested > available {
        println!(
            "[env] NUM_TASKS={requested} exceeds {available} usable CPU(s); \
             clamping to {available}"
        );
        available
    } else {
        requested
    }
}

/// Expected hashes to find a block at this target, from the top 8 bytes only.
fn expected_hashes(target: &[u8; 32]) -> f64 {
    let mut top: f64 = 0.0;
    for b in &target[..8] {
        top = top * 256.0 + *b as f64;
    }
    let t = top * 2f64.powi(192);
    if t <= 0.0 {
        f64::INFINITY
    } else {
        2f64.powi(256) / t
    }
}

fn human_secs(s: f64) -> String {
    if !s.is_finite() {
        return "never".into();
    }
    if s >= 86_400.0 {
        format!("{:.1}d", s / 86_400.0)
    } else if s >= 3_600.0 {
        format!("{:.1}h", s / 3_600.0)
    } else if s >= 60.0 {
        format!("{:.1}m", s / 60.0)
    } else {
        format!("{:.0}s", s)
    }
}

/// Where this template stands relative to the minimum-difficulty flip point.
fn min_diff_state(t: &BlockTemplate, now: u64) -> String {
    if !MIN_DIFFICULTY_RULE.load(Ordering::Relaxed) {
        return "n/a".into();
    }
    if t.parent_time == 0 {
        return "unknown".into();
    }
    let boundary = min_difficulty_boundary(t.parent_time, t.target_time);
    if now > boundary {
        "eligible".into()
    } else {
        format!("in {}", human_secs((boundary + 1 - now) as f64))
    }
}

async fn mine_and_submit(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
) -> MinerResult<()> {
    let Some(mined) = mine_loop().await? else {
        return Ok(());
    };

    let mine::MinedBlock {
        header,
        coinbase,
        raw_txs,
        payout_address,
        height,
        sizelimit,
    } = mined;

    let block_bytes = build_full_block(header, &coinbase, &raw_txs, sizelimit)?;
    println!(
        "[sub] candidate height={height} size={} bytes",
        block_bytes.len()
    );

    match submit_block(client, rpc_url, rpc_user, rpc_pass, block_bytes).await {
        Ok(SubmitOutcome::Accepted) => {
            println!("[sub] accepted at height {height}");
            REFRESH.notify_one();
            if let Err(e) = advance_after_block(&payout_address).await {
                eprintln!("[sub] warning: failed to advance payout address: {e}");
            }
        }
        Ok(SubmitOutcome::Duplicate) => {
            // The node already had this block. It may be our own earlier
            // submission whose response was lost, in which case this address is
            // already on chain; it may also be another miner's block. We cannot
            // tell the two apart, so the cursor is held rather than risk
            // skipping a payout address that never got used.
            println!("[sub] duplicate at height {height}: already known, payout address kept");
            REFRESH.notify_one();
        }
        Err(e) => {
            // Any rejection can mean the tip moved under us, and a submission
            // that timed out is the case where it certainly did. A rejected
            // candidate is a once-in-a-lifetime event, so refetching
            // unconditionally costs nothing; sitting on a dead template until
            // the longpoll expires does not.
            let msg = e.to_string();
            let suffix = if e.downcast_ref::<RpcTimeout>().is_some() {
                // Not a rejection: the request never completed, so the block may
                // have been accepted. Do not claim the tip moved.
                " (outcome unknown)"
            } else if msg.contains("bad-prevblk") || msg.contains("inconclusive") {
                " (tip moved)"
            } else {
                ""
            };
            eprintln!("[sub] submit failed{suffix}: {msg}");
            REFRESH.notify_one();
        }
    }

    Ok(())
}

fn spawn_stats(bench_rate: f64) {
    tokio::spawn(async move {
        let mut last = 0u64;
        let mut ticker = tokio::time::interval(Duration::from_secs(10));
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let total = mine::HASH_COUNT.load(Ordering::Relaxed);
            let rate = (total - last) as f64 / 10.0 / 1e6;
            last = total;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let snapshot = GLOBAL_TEMPLATE.read().await;
            let (height, tau, find_secs, min_diff) = match snapshot.as_ref() {
                Some((t, _)) => {
                    let find = u32::from_str_radix(&t.bits, 16)
                        .ok()
                        .and_then(utils::bits_to_target)
                        .filter(|_| rate > 0.0)
                        .map(|target| expected_hashes(&target) / (rate * 1e6))
                        .unwrap_or(f64::INFINITY);
                    (Some(t.height), t.target_time, find, min_diff_state(t, now))
                }
                None => (
                    None,
                    TARGET_TIME.load(Ordering::Relaxed),
                    f64::INFINITY,
                    "n/a".to_string(),
                ),
            };
            drop(snapshot);

            let payout_set = PAYOUT_ADDRESS.read().await.is_some();

            // This miner's own expected find time, so the polling-lag estimate
            // below stays meaningful at any difficulty. `tau` is the network
            // cadence and is reported separately so the two are not confused.
            let waste = if find_secs.is_finite() && find_secs > 0.0 {
                (0.5 / find_secs * 100.0).min(100.0)
            } else {
                0.0
            };

            println!(
                "[stats] {rate:.2} MH/s bench={:.2} MH/s cumulative={total} height={height:?} \
                 tau={tau}s find~{} waste~{waste:.2}% min_diff={min_diff} time={} \
                 payout_set={payout_set}",
                bench_rate / 1e6,
                human_secs(find_secs),
                TimestampMode::name(mine::LAST_TIMESTAMP_MODE.load(Ordering::Relaxed)),
            );
        }
    });
}

#[tokio::main]
async fn main() -> MinerResult<()> {
    let env_path = load_dotenv()?;
    println!("[env] loaded {}", env_path.display());

    let num_tasks = resolve_num_tasks(
        var("NUM_TASKS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .max(1),
    );
    let rpc_url = required("RPC_URL")?;
    let rpc_user = clean(required("RPC_USER")?);
    let rpc_pass = clean(required("RPC_PASS")?);
    let strings_file = required("STRINGS_FILE")?;

    println!(
        "[env] rpc_url={rpc_url} rpc_user={rpc_user:?} pass_len={} num_tasks={num_tasks}",
        rpc_pass.len()
    );

    let client = Arc::new(Client::builder().timeout(Duration::from_secs(30)).build()?);

    preflight(&client, &rpc_url, &rpc_user, &rpc_pass).await?;
    load_sentences_from_file(&strings_file)?;
    // Before any worker starts: validates the whole address file and installs
    // the first payout address, so no worker ever sees `None` at startup.
    init_addresses().await?;

    // Benchmark on a blocking thread before any worker starts, then size the
    // nonce batch so a worker notices a new template within ~50 ms.
    let bench_rate = tokio::task::spawn_blocking(|| mine::bench_hashrate(300)).await?;
    let batch = mine::configure_batch(bench_rate);
    println!(
        "[bench] {:.2} MH/s/thread -> nonce batch {batch} (~{:.0} ms)",
        bench_rate / 1e6,
        batch as f64 / bench_rate * 1000.0
    );

    update_block(
        Arc::clone(&client),
        rpc_url.clone(),
        rpc_user.clone(),
        rpc_pass.clone(),
    );
    spawn_stats(bench_rate);
    spawn_file_watcher();

    #[cfg(windows)]
    {
        use std::thread;

        for _ in 0..num_tasks {
            let client = Arc::clone(&client);
            let rpc_url = rpc_url.clone();
            let rpc_user = rpc_user.clone();
            let rpc_pass = rpc_pass.clone();

            thread::spawn(move || {
                // current_thread: the nonce search already runs on the blocking
                // pool, so a multi-thread runtime per OS thread would just
                // oversubscribe the CPU by num_cores.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build runtime");
                loop {
                    if let Err(e) =
                        rt.block_on(mine_and_submit(&client, &rpc_url, &rpc_user, &rpc_pass))
                    {
                        eprintln!("[worker] failed: {e}");
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            });
        }
    }

    #[cfg(not(windows))]
    {
        for _ in 0..num_tasks {
            let client = Arc::clone(&client);
            let rpc_url = rpc_url.clone();
            let rpc_user = rpc_user.clone();
            let rpc_pass = rpc_pass.clone();

            tokio::spawn(async move {
                loop {
                    if let Err(e) = mine_and_submit(&client, &rpc_url, &rpc_user, &rpc_pass).await {
                        eprintln!("[worker] failed: {e}");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            });
        }
    }

    futures::future::pending::<()>().await;
    Ok(())
}
