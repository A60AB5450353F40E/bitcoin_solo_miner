use crate::utils::{MinerResult, rpc_call_timeout};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Deserialize, Clone)]
pub struct BlockTemplateTx {
    /// Final serialized transaction, hex, byte-for-byte. Never decoded.
    pub data: String,
    #[serde(default)]
    pub txid: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    /// 1-based indices into `transactions` that must precede this tx.
    #[serde(default)]
    pub depends: Vec<u64>,
    /// Satoshis. Absent means unknown: never assume there are no fees.
    #[serde(default)]
    pub fee: Option<i64>,
    /// Sigcheck count, as counted for block limits.
    #[serde(default)]
    pub sigops: Option<u64>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct CoinbaseAux {
    /// Hex that must be embedded in the coinbase scriptSig.
    #[serde(default)]
    pub flags: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BlockTemplate {
    pub version: i32,
    pub previousblockhash: String,
    pub height: u64,
    pub bits: String,
    pub curtime: u64,
    pub mintime: Option<u64>,
    /// Maximum allowable coinbase input: subsidy + fees of the whole tx set.
    pub coinbasevalue: u64,
    pub target: Option<String>,
    pub noncerange: Option<String>,
    pub sizelimit: Option<u64>,
    pub sigoplimit: Option<u64>,
    #[serde(default)]
    pub mutable: Vec<String>,
    #[serde(default)]
    pub coinbaseaux: Option<CoinbaseAux>,
    #[serde(default)]
    pub transactions: Vec<BlockTemplateTx>,
    /// Consensus target block time, in seconds, for **this** block.
    ///
    /// `getblocktemplate` describes the block being built, so this is that
    /// block's cadence, not the tip's. It is the value the minimum-difficulty
    /// threshold for this block is keyed to, and the same expression the node
    /// evaluates as `pindexPrev->GetTickDeltaForNextBlock()`.
    ///
    /// Required on purpose: a node without this field produces a template this
    /// miner will not mine, rather than one it guesses a cadence for.
    pub target_time: u64,
    /// Parent block timestamp, filled in by `block_updater` after the fetch.
    /// 0 means unknown, and callers must then fall back to `curtime`.
    #[serde(skip)]
    pub parent_time: u64,
}

/// BCHN v29 `template_request` has `mode`, `capabilities`, `longpollid`,
/// `checkvalidity`, `ignorecache` — and no `rules` key, so none is sent.
///
/// `longpollid` is the current best block hash, not an opaque token handed back
/// by a previous response: the node answers immediately once that block is no
/// longer the tip.
fn template_request(longpoll_tip: Option<&str>) -> Value {
    let mut request = json!({
        "capabilities": ["coinbasevalue", "longpoll"],
    });
    if let Some(tip) = longpoll_tip {
        request["longpollid"] = json!(tip);
    }
    request
}

pub async fn fetch_block_template(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    longpoll_tip: Option<&str>,
    timeout: Option<Duration>,
) -> MinerResult<BlockTemplate> {
    let result = rpc_call_timeout(
        client,
        rpc_url,
        rpc_user,
        rpc_pass,
        "getblocktemplate",
        json!([template_request(longpoll_tip)]),
        timeout,
    )
    .await?;

    if result.is_null() {
        return Err("getblocktemplate returned a null result".into());
    }

    let template: BlockTemplate = serde_json::from_value(result)
        .map_err(|e| format!("unexpected block template shape: {e}"))?;

    Ok(template)
}

/// Tick-schedule conformance: `1 <= tau <= 600` and `tau` divides 600.
///
/// Reported rather than fatal: the node takes `bits`, `coinbasevalue` and
/// `sizelimit` from itself, so a surprising cadence costs timestamp accuracy,
/// never block validity. The conformance flag also gates the timestamp ceiling,
/// because an over-estimated tau is the one direction that can cost a block.
pub fn check_target_time(tau: u64) -> Option<String> {
    if tau == 0 {
        return Some("target_time is 0".into());
    }
    if !(1..=600).contains(&tau) || 600 % tau != 0 {
        return Some(format!(
            "target_time={tau} does not conform to the tick schedule \
             (need 1 <= tau <= 600 and tau | 600); timestamps pinned to curtime"
        ));
    }
    None
}

#[derive(Debug, Deserialize)]
struct HeaderTime {
    time: u64,
}

/// One extra call per *tip*, not per template: the timestamp policy needs the
/// parent's own header time to place the minimum-difficulty flip point, and
/// `mintime` (parent median time past) is not it — it can be ~600 s older.
///
/// Best effort by design. `Ok(None)`/`Err` leaves `parent_time == 0`, which
/// makes the policy pin timestamps to `curtime`: always consistent with the
/// template's `bits`, just less fresh.
pub async fn fetch_parent_time(
    client: &Client,
    rpc_url: &str,
    rpc_user: &str,
    rpc_pass: &str,
    prev_hash: &str,
) -> MinerResult<Option<u64>> {
    let result = rpc_call_timeout(
        client,
        rpc_url,
        rpc_user,
        rpc_pass,
        "getblockheader",
        json!([prev_hash]),
        Some(Duration::from_secs(10)),
    )
    .await?;
    if result.is_null() {
        return Ok(None);
    }
    let header: HeaderTime = serde_json::from_value(result)
        .map_err(|e| format!("unexpected getblockheader shape: {e}"))?;
    Ok(Some(header.time))
}
