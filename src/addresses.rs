use crate::PAYOUT_ADDRESS;
use crate::utils::{MinerResult, payout_script};
use bitcoin::hashes::{Hash, sha256};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// How often to notice that the operator edited the address file underneath us.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);
/// Cap on per-line warnings so a file full of `bc1q…` cannot flood the log.
const MAX_LINE_WARNINGS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Walk the list once, then stop mining. Default: never reuse a payout script.
    Once,
    /// Walk the list and wrap around.
    Loop,
    /// Pick a random entry for each block.
    Random,
}

impl Mode {
    fn parse(raw: &str) -> MinerResult<Mode> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "once" => Ok(Mode::Once),
            "loop" => Ok(Mode::Loop),
            "random" => Ok(Mode::Random),
            other => Err(format!("ADDRESS_MODE={other:?} is not one of once|loop|random").into()),
        }
    }

    fn is_random(self) -> bool {
        matches!(self, Mode::Random)
    }
}

/// Persisted cursor. `file_len` + `sha256` describe the file as it was when this
/// state was written, which is what lets a later run tell "the operator appended
/// a few addresses" from "the operator replaced the list".
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AddrState {
    pub file_len: u64,
    /// sha256 (hex) of the whole file at that moment.
    pub sha256: String,
    /// Index of the address to use next.
    pub next_index: usize,
    /// `once` mode only: every address in the list has been used.
    pub exhausted: bool,
}

pub struct AddressBook {
    path: PathBuf,
    state_path: PathBuf,
    mode: Mode,
    addresses: Vec<String>,
    state: AddrState,
}

/// Single owner of the address list. Held only for short, synchronous
/// operations (never across an `.await`) so the guard stays out of futures.
pub static BOOK: Lazy<Mutex<Option<AddressBook>>> = Lazy::new(|| Mutex::new(None));

fn file_hash(bytes: &[u8]) -> String {
    hex::encode(sha256::Hash::hash(bytes).to_byte_array())
}

/// True when `bytes` is `previous` with lines appended: the leading
/// `prev.file_len` bytes still hash to what we recorded.
fn is_append(prev: &AddrState, bytes: &[u8]) -> bool {
    bytes.len() as u64 > prev.file_len
        && (prev.file_len as usize) <= bytes.len()
        && file_hash(&bytes[..prev.file_len as usize]) == prev.sha256
}

fn read_state(path: &Path) -> Option<AddrState> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<AddrState>(&bytes) {
        Ok(state) => Some(state),
        Err(e) => {
            eprintln!(
                "[addr] ignoring unreadable state file {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Parses and validates. Invalid lines are skipped (a `bc1q…` payout would be
/// anyone-can-spend on BCH, so it must never reach a coinbase), duplicates are
/// dropped, and the first few problems are reported with a line number.
fn parse_addresses(bytes: &[u8]) -> MinerResult<(Vec<String>, usize)> {
    let text =
        std::str::from_utf8(bytes).map_err(|e| format!("address file is not valid UTF-8: {e}"))?;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut skipped = 0usize;
    let mut reported = 0usize;

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !seen.insert(line.to_string()) {
            continue;
        }
        match payout_script(line) {
            Ok(_) => out.push(line.to_string()),
            Err(e) => {
                skipped += 1;
                if reported < MAX_LINE_WARNINGS {
                    eprintln!("[addr] line {}: {e}", i + 1);
                    reported += 1;
                }
            }
        }
    }

    Ok((out, skipped))
}

impl AddressBook {
    fn load() -> MinerResult<Self> {
        let path = PathBuf::from(
            dotenvy::var("PAYOUT_ADDRESSES")
                .map_err(|_| "PAYOUT_ADDRESSES missing from environment/.env")?,
        );
        let state_path = match dotenvy::var("ADDRESSES_STATE") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => PathBuf::from(format!("{}.state", path.display())),
        };
        let mode = match dotenvy::var("ADDRESS_MODE") {
            Ok(v) if !v.trim().is_empty() => Mode::parse(&v)?,
            _ => Mode::Once,
        };

        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let (addresses, skipped) = parse_addresses(&bytes)?;
        if addresses.is_empty() {
            return Err(format!(
                "{} has no usable payout addresses (need legacy P2PKH 1…/P2SH 3…, \
                 not bc1q and not cashaddr)",
                path.display()
            )
            .into());
        }
        if skipped > 0 {
            eprintln!("[addr] WARNING: skipped {skipped} unusable address line(s)");
        }

        let hash = file_hash(&bytes);
        let n = addresses.len();
        let state = match read_state(&state_path) {
            Some(prev) => reconcile(prev, &bytes, &hash, n, mode),
            None => AddrState {
                file_len: bytes.len() as u64,
                sha256: hash,
                next_index: if mode.is_random() {
                    fastrand::usize(..n)
                } else {
                    0
                },
                exhausted: false,
            },
        };

        Ok(AddressBook {
            path,
            state_path,
            mode,
            addresses,
            state,
        })
    }

    fn persist(&self) -> MinerResult<()> {
        let json = serde_json::to_vec_pretty(&self.state)?;
        let tmp = self.state_path.with_extension("state.tmp");
        if let Some(parent) = self.state_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        // tmp + fsync + rename: a crash mid-write cannot leave a truncated or
        // zero-length cursor, only the previous one.
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }

    fn persist_quiet(&self) {
        if let Err(e) = self.persist() {
            eprintln!("[addr] warning: could not persist cursor: {e}");
        }
    }

    /// The address the next coinbase should pay, or `None` when mining must idle.
    fn current(&self) -> Option<String> {
        let n = self.addresses.len();
        if n == 0 {
            return None;
        }
        match self.mode {
            Mode::Once => {
                if self.state.exhausted || self.state.next_index >= n {
                    None
                } else {
                    Some(self.addresses[self.state.next_index].clone())
                }
            }
            Mode::Loop | Mode::Random => Some(self.addresses[self.state.next_index % n].clone()),
        }
    }

    /// Moves the cursor past `used` after a block was accepted, persists it, and
    /// returns the address to mine with next.
    fn advance(&mut self, used: &str) -> Option<String> {
        let n = self.addresses.len();
        if n == 0 {
            self.state.exhausted = true;
            return None;
        }

        let idx = self
            .addresses
            .iter()
            .position(|a| a == used)
            .unwrap_or(self.state.next_index.min(n - 1));

        let next = match self.mode {
            Mode::Once => idx + 1,
            Mode::Loop => (idx + 1) % n,
            Mode::Random => {
                if n == 1 {
                    0
                } else {
                    let mut r = fastrand::usize(..n);
                    while r == idx {
                        r = fastrand::usize(..n);
                    }
                    r
                }
            }
        };

        self.state.next_index = next;
        self.state.exhausted = matches!(self.mode, Mode::Once) && next >= n;
        if self.state.exhausted {
            println!(
                "[addr] list exhausted after {n} payout(s); mining stops. Add addresses to {} \
                 or set ADDRESS_MODE=loop to reuse them.",
                self.path.display()
            );
        }
        self.persist_quiet();
        self.current()
    }

    /// Re-reads the file. Returns Ok(true) when it changed and the cursor moved.
    fn reload_from_disk(&mut self) -> MinerResult<bool> {
        let bytes = std::fs::read(&self.path)
            .map_err(|e| format!("cannot re-read {}: {e}", self.path.display()))?;
        let hash = file_hash(&bytes);
        if hash == self.state.sha256 && bytes.len() as u64 == self.state.file_len {
            return Ok(false);
        }

        let (addresses, skipped) = parse_addresses(&bytes)?;
        if addresses.is_empty() {
            return Err(format!(
                "{} no longer has usable addresses; keeping the previous list",
                self.path.display()
            )
            .into());
        }
        if skipped > 0 {
            eprintln!("[addr] WARNING: skipped {skipped} unusable address line(s)");
        }

        let n = addresses.len();
        let prev = self.state.clone();
        self.addresses = addresses;
        self.state = reconcile(prev, &bytes, &hash, n, self.mode);
        self.persist_quiet();
        Ok(true)
    }
}

fn reconcile(prev: AddrState, bytes: &[u8], hash: &str, n: usize, mode: Mode) -> AddrState {
    let len = bytes.len() as u64;

    if prev.sha256 == hash && prev.file_len == len {
        return AddrState {
            next_index: prev.next_index.min(n),
            exhausted: prev.exhausted,
            ..prev
        };
    }

    if is_append(&prev, bytes) {
        println!(
            "[addr] address file grew to {n} entries; keeping cursor at {}",
            prev.next_index
        );
        return AddrState {
            file_len: len,
            sha256: hash.to_string(),
            next_index: prev.next_index.min(n),
            exhausted: false,
        };
    }

    println!("[addr] address file changed; resetting cursor");
    AddrState {
        file_len: len,
        sha256: hash.to_string(),
        next_index: if mode.is_random() {
            fastrand::usize(..n.max(1))
        } else {
            0
        },
        exhausted: false,
    }
}

/// Loads the list, validates it, and installs the first payout address. Called
/// once from `main` before any worker is spawned.
pub async fn init_addresses() -> MinerResult<()> {
    let book = AddressBook::load()?;
    let n = book.addresses.len();
    let selection = book.current();
    println!(
        "[addr] {n} payout address(es), mode={:?}, next_index={}{} first={}",
        book.mode,
        book.state.next_index,
        if book.state.exhausted {
            " (exhausted)"
        } else {
            ""
        },
        selection.as_deref().unwrap_or("<none>"),
    );
    book.persist_quiet();

    {
        let mut guard = BOOK.lock().map_err(|_| "address book lock poisoned")?;
        *guard = Some(book);
    }

    if selection.is_none() {
        println!("[addr] NOTE: list is already exhausted; mining will stay idle");
    }
    *PAYOUT_ADDRESS.write().await = selection;
    Ok(())
}

/// Called after `submitblock` accepted a block. Advances the cursor and swaps
/// the payout address; nothing else is written to the address file.
pub async fn advance_after_block(used: &str) -> MinerResult<()> {
    let selection = {
        let mut guard = BOOK.lock().map_err(|_| "address book lock poisoned")?;
        let book = guard.as_mut().ok_or("address book not initialised")?;
        book.advance(used)
    };

    match &selection {
        Some(addr) => println!("[addr] next payout {addr}"),
        None => println!("[addr] no payout address left; workers will idle"),
    }
    *PAYOUT_ADDRESS.write().await = selection;
    Ok(())
}

/// Notices operator edits (appended addresses, a reloaded file) without ever
/// rewriting the address file itself.
pub fn spawn_file_watcher() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RELOAD_INTERVAL);
        ticker.tick().await;
        let mut warned_poisoned = false;

        loop {
            ticker.tick().await;

            let updated = {
                // A poisoned lock means another thread panicked mid-mutation.
                // Skip the tick instead of returning, which would silently
                // disable reloads for the rest of the process's life.
                let mut guard = match BOOK.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        if !warned_poisoned {
                            eprintln!("[addr] address book lock poisoned; reload skipped");
                            warned_poisoned = true;
                        }
                        continue;
                    }
                };
                match guard.as_mut() {
                    Some(book) => match book.reload_from_disk() {
                        Ok(true) => Some(book.current()),
                        Ok(false) => None,
                        Err(e) => {
                            eprintln!("[addr] reload failed: {e}");
                            None
                        }
                    },
                    None => None,
                }
            };

            if let Some(sel) = updated {
                println!(
                    "[addr] payout is now {}",
                    sel.as_deref().unwrap_or("<none: exhausted>")
                );
                *PAYOUT_ADDRESS.write().await = sel;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_for(bytes: &[u8]) -> AddrState {
        AddrState {
            file_len: bytes.len() as u64,
            sha256: file_hash(bytes),
            next_index: 3,
            exhausted: false,
        }
    }

    #[test]
    fn append_keeps_the_cursor() {
        let base = b"addr_a\naddr_b\n";
        let prev = state_for(base);
        let mut grown = base.to_vec();
        grown.extend_from_slice(b"addr_c\n");
        assert!(is_append(&prev, &grown));
        assert_eq!(
            reconcile(prev.clone(), &grown, &file_hash(&grown), 3, Mode::Once).next_index,
            3
        );
    }

    #[test]
    fn unchanged_file_is_not_an_append() {
        let base = b"addr_a\naddr_b\n";
        let prev = state_for(base);
        assert!(!is_append(&prev, base));
    }

    #[test]
    fn rewrite_and_shrink_reset_the_cursor() {
        let base = b"addr_a\naddr_b\naddr_c\naddr_d\n";
        let prev = state_for(base);

        let rewritten = b"addr_a\naddr_z\naddr_c\naddr_w\n";
        let next = reconcile(
            prev.clone(),
            rewritten,
            &file_hash(rewritten),
            4,
            Mode::Once,
        );
        assert_eq!(next.next_index, 0);
        assert!(!next.exhausted);

        let shrunk = b"addr_a\n";
        let next = reconcile(prev, shrunk, &file_hash(shrunk), 1, Mode::Once);
        assert_eq!(next.next_index, 0);
    }

    #[test]
    fn append_clears_exhaustion() {
        let base = b"addr_a\n";
        let mut prev = state_for(base);
        prev.exhausted = true;
        prev.next_index = 1;

        let mut grown = base.to_vec();
        grown.extend_from_slice(b"addr_b\n");
        let next = reconcile(prev, &grown, &file_hash(&grown), 2, Mode::Once);
        assert!(!next.exhausted);
        assert_eq!(next.next_index, 1);
    }
}
