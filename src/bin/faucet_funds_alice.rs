//! Alice asks for 1 BTK and 1 BTC from the two faucets, and gets both.
//!
//! Mission 11.1's headline. Two chains, two faucets, one asker.
//!
//! **Run against locally spun regtest, not the live signets.** The faucet
//! cannot tell the difference — it holds RPC to a `bitcoind` and pays out of
//! a wallet, and the chain type is invisible to it except for block timing.
//! So this is a faithful test of everything the faucet does, and
//! confirmations are mined on demand rather than waited for.
//!
//! Shape: one node per chain, two wallets each. `testwallet` is the miner,
//! which the faucet spends from; `treasury` is what Alice fills. That is
//! cheaper than four nodes and covers the whole path. What it does not
//! exercise is network separation between Alice's node and the faucet's,
//! which is not what this scenario is for.
//!
//! Alice is the test itself here. The `nostr-faucet-client` binary is
//! Mission 11.2; this proves the server.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{Method, Request, RequestParams, Response};
use serde_json::json;

use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::process::ManagedChild;
use dln_e2e_harness::{relay, util};

/// Past coinbase maturity, so the miner has something spendable.
const PREMINE: u64 = 110;

/// One coin, in sats. What Alice asks each faucet for.
const ONE_COIN_SAT: u64 = 100_000_000;

/// Seconds, not a week. A "one coin a week" policy cannot be tested against
/// real time, which is why the window is configuration in the first place.
const WINDOW_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => {
            println!("\n=== PASS ===");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

/// One chain's worth of the setup.
struct Chain {
    label: &'static str,
    node: BitcoindHarness,
    faucet_pubkey: PublicKey,
    _faucet: ManagedChild,
    miner_addr: String,
}

async fn run() -> Result<()> {
    let out = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("faucet-e2e")),
    );
    std::fs::create_dir_all(&out)?;
    tracing::info!("output: {}", out.display());

    let faucet_bin = faucet_binary()?;
    let (_relay_container, relay_url) = relay::start_relay().await;

    // ── Two chains, each with a miner and a treasury ─────────────────────
    tracing::info!("Step 1: two chains, each with a funded miner");
    let core = BitcoindHarness::start().await;
    let knots = BitcoindHarness::start_knots().await;

    let alice = Keys::generate();
    tracing::info!("  alice is {}", alice.public_key());

    let mut chains = Vec::new();
    for (label, node) in [("btc.regtest", core), ("btk.regtest", knots)] {
        let miner_addr = node.get_new_address().await;
        node.mine_blocks(PREMINE, &miner_addr).await;
        node.create_wallet("treasury").await;

        let (faucet_pubkey, child) =
            start_faucet(label, &node, &relay_url, &faucet_bin, &out).await?;

        chains.push(Chain { label, node, faucet_pubkey, _faucet: child, miner_addr });
    }

    // ── The headline ─────────────────────────────────────────────────────
    tracing::info!("Step 2: alice asks each faucet for 1 coin");
    for c in &chains {
        let before = treasury_balance(&c.node).await?;
        let addr = treasury_address(&c.node).await?;
        let uri = format!("bitcoin:{addr}?amount=1");

        let resp = ask(&relay_url, &alice, &c.faucet_pubkey, &uri).await?;
        anyhow::ensure!(
            resp.error.is_none(),
            "{}: the faucet refused a first request from a new key, which the open \
             policy should allow: {:?}",
            c.label,
            resp.error
        );

        // A txid is not money. Confirm it.
        c.node.mine_blocks(1, &c.miner_addr).await;
        let after = treasury_balance(&c.node).await?;
        anyhow::ensure!(
            after >= before + 1.0,
            "{}: treasury went {before} -> {after}, expected at least 1 coin more",
            c.label
        );
        tracing::info!("  {}: treasury {before} -> {after}", c.label);
    }

    // ── The quota, which is what makes it a policy rather than a tap ─────
    tracing::info!("Step 3: a second coin inside the window is refused, and says why");
    for c in &chains {
        let addr = treasury_address(&c.node).await?;
        let uri = format!("bitcoin:{addr}?amount=1");
        let resp = ask(&relay_url, &alice, &c.faucet_pubkey, &uri).await?;

        let err = resp.error.context(format!(
            "{}: a second coin inside the window was paid — the quota is not enforced",
            c.label
        ))?;
        anyhow::ensure!(
            err.message.contains("quota"),
            "{}: refused, but not for the quota: {}",
            c.label,
            err.message
        );
        anyhow::ensure!(
            err.message.contains("try again in"),
            "{}: the refusal does not say when to come back: {}",
            c.label,
            err.message
        );
        tracing::info!("  {}: refused — {}", c.label, err.message);
    }

    // ── Capabilities, so a client can know before it asks ────────────────
    tracing::info!("Step 4: each faucet advertises onchain and nothing else");
    for c in &chains {
        let resp = get_info(&relay_url, &alice, &c.faucet_pubkey).await?;
        let value = serde_json::to_value(&resp)?;
        let methods = value
            .pointer("/result/bip321_methods")
            .context("get_info carries no bip321_methods")?;
        let listed: Vec<String> = methods
            .as_array()
            .context("bip321_methods is not an array")?
            .iter()
            .filter_map(|m| m.get("method").and_then(|x| x.as_str()).map(String::from))
            .collect();
        anyhow::ensure!(
            listed == vec!["onchain".to_string()],
            "{}: advertises {listed:?}, expected only onchain — a faucet with no \
             Lightning wallet should say so before a client asks",
            c.label
        );
        tracing::info!("  {}: bip321_methods = {listed:?}", c.label);
    }

    Ok(())
}

// ── the faucet under test ────────────────────────────────────────────────

fn faucet_binary() -> Result<String> {
    if let Ok(p) = std::env::var("NOSTR_FAUCET_BINARY") {
        return Ok(p);
    }
    let repo = std::env::var("NOSTR_FAUCET_REPO")
        .unwrap_or_else(|_| format!("{}/git/nostr-faucet", std::env::var("HOME").unwrap()));
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "nostr-faucet"])
        .current_dir(&repo)
        .status()
        .with_context(|| format!("cannot build nostr-faucet in {repo}"))?;
    anyhow::ensure!(status.success(), "building nostr-faucet failed");
    Ok(format!("{repo}/target/debug/nostr-faucet"))
}

async fn start_faucet(
    label: &str,
    node: &BitcoindHarness,
    relay_url: &str,
    binary: &str,
    out: &std::path::Path,
) -> Result<(PublicKey, ManagedChild)> {
    let keys = Keys::generate();
    let dir = out.join(label);
    std::fs::create_dir_all(&dir)?;
    let cfg_path = dir.join("faucet.toml");

    // Window and cap in seconds and coins, not weeks — the reason both are
    // configuration rather than constants.
    std::fs::write(
        &cfg_path,
        format!(
            r#"
[nostr]
relay = "{relay_url}"
secret_key = "{}"

[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = {}
rpc_user = "{}"
rpc_password = "{}"
wallet = "testwallet"
chain_label = "{label}"

[policy]
per_key_sat = {ONE_COIN_SAT}
window_secs = {WINDOW_SECS}
total_cap_sat = {}
max_requests_per_window = 10
paused = false
"#,
            keys.secret_key().to_secret_hex(),
            node.rpc_port(),
            node.rpc_user(),
            node.rpc_password(),
            ONE_COIN_SAT * 5,
        ),
    )?;

    let child = ManagedChild::spawn(
        &format!("{label}-faucet"),
        binary,
        &[cfg_path.to_str().unwrap()],
        &dir,
    )?;

    // The faucet checks its miner at startup, so give it a moment to fail
    // loudly if it is going to.
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok((keys.public_key(), child))
}

// ── Alice, who is the test ───────────────────────────────────────────────

async fn ask(relay: &str, alice: &Keys, faucet: &PublicKey, uri: &str) -> Result<Response> {
    send(
        relay,
        alice,
        faucet,
        Request {
            method: Method::PayBip321,
            params: RequestParams::PayBip321(
                nwc::nostr::nips::nip47::PayBip321Request { uri: uri.to_string() },
            ),
        },
    )
    .await
}

async fn get_info(relay: &str, alice: &Keys, faucet: &PublicKey) -> Result<Response> {
    send(
        relay,
        alice,
        faucet,
        Request { method: Method::GetInfo, params: RequestParams::GetInfo },
    )
    .await
}

/// One NWC round trip. Written here rather than borrowed from the harness so
/// this suite does not depend on another repo's internals.
async fn send(
    relay: &str,
    alice: &Keys,
    faucet: &PublicKey,
    req: Request,
) -> Result<Response> {
    let client = Client::builder().signer(alice.clone()).build();
    client.add_relay(relay).await?;
    client.connect().await;

    let sub = Filter::new()
        .kind(Kind::WalletConnectResponse)
        .pubkey(alice.public_key())
        .since(Timestamp::now());
    client.subscribe(sub).await?;

    let encrypted = nip04::encrypt(
        alice.secret_key(),
        faucet,
        serde_json::to_string(&req)?,
    )?;
    let event = EventBuilder::new(Kind::WalletConnectRequest, encrypted)
        .tag(Tag::public_key(*faucet));
    client.send_event_builder(event).await?;

    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "no answer from the faucet in 30s — a faucet that is down and a faucet \
             that refused should not look the same, so this is a failure"
        );
        let next = tokio::time::timeout(remaining, notifications.next()).await;
        let Ok(Some(ClientNotification::Event { event, .. })) = next else { continue };
        if event.kind != Kind::WalletConnectResponse || event.pubkey != *faucet {
            continue;
        }
        let plaintext = nip04::decrypt(alice.secret_key(), faucet, &event.content)?;
        let _ = client.disconnect().await;
        return Ok(serde_json::from_str(&plaintext)?);
    }
}

// ── the treasury Alice is filling ────────────────────────────────────────

async fn treasury_address(node: &BitcoindHarness) -> Result<String> {
    // bech32 explicitly: Knots defaults getnewaddress to legacy P2PKH, which
    // mission 10.1 found the hard way.
    node.rpc_wallet("treasury", "getnewaddress", json!(["", "bech32"]))
        .await
        .map_err(|e| anyhow::anyhow!("treasury getnewaddress: {e}"))?
        .as_str()
        .map(String::from)
        .context("getnewaddress did not return a string")
}

async fn treasury_balance(node: &BitcoindHarness) -> Result<f64> {
    node.rpc_wallet("treasury", "getbalance", json!([]))
        .await
        .map_err(|e| anyhow::anyhow!("treasury getbalance: {e}"))?
        .as_f64()
        .context("getbalance did not return a number")
}
