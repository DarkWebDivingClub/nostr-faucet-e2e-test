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
use nostr_faucet_e2e_test::*;
use nostr_sdk::prelude::*;
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
    // The owner is the only key whose grants a faucet accepts. It replaces
    // `control_pubkey`, and it is what the faucet's policy is written with
    // now: there is no policy file any more.
    let owner = Keys::generate();
    tracing::info!("  alice is {}", alice.public_key());
    tracing::info!("  owner is {}", owner.public_key());

    let mut chains = Vec::new();
    for (label, node) in [("btc.regtest", core), ("btk.regtest", knots)] {
        let miner_addr = node.get_new_address().await;
        node.mine_blocks(PREMINE, &miner_addr).await;
        node.create_wallet("treasury").await;

        let (faucet_pubkey, child) =
            start_faucet(label, &node, &relay_url, &faucet_bin, &out, &owner).await?;

        // One coin per window, as a grant rather than a config key. This is
        // the whole of what used to be `[policy] per_key_sat/window_secs`.
        publish_grant(
            &owner,
            &faucet_pubkey,
            &alice.public_key(),
            &format!(
                r#"{{"methods":{{"OTHERS":{{}}}},"quota":{{"amount":{ONE_COIN_SAT},"per_secs":{WINDOW_SECS},"max_capacity":{ONE_COIN_SAT}}}}}"#
            ),
            &relay_url,
        )
        .await?;

        chains.push(Chain { label, node, faucet_pubkey, _faucet: child, miner_addr });
    }

    // ── The headline ─────────────────────────────────────────────────────
    tracing::info!("Step 2: alice asks each faucet for 1 coin");
    for c in &chains {
        let before = treasury_balance(&c.node).await?;
        let addr = treasury_address(&c.node).await?;

        let resp = ask(&relay_url, &alice, &c.faucet_pubkey, &addr, ONE_COIN_SAT).await?;
        anyhow::ensure!(
            refusal(&resp).is_none(),
            "{}: the faucet refused a first request from a granted key: {:?}",
            c.label,
            refusal(&resp)
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
        let resp = ask(&relay_url, &alice, &c.faucet_pubkey, &addr, ONE_COIN_SAT).await?;

        let message = refusal(&resp).context(format!(
            "{}: a second coin inside the window was paid — the quota is not enforced",
            c.label
        ))?;
        anyhow::ensure!(
            message.to_lowercase().contains("quota"),
            "{}: refused, but not for the quota: {message}",
            c.label
        );
        tracing::info!("  {}: refused — {message}", c.label);
    }

    // ── Capabilities, so a client can know before it asks ────────────────
    tracing::info!("Step 4: each faucet advertises exactly what it implements");
    for c in &chains {
        let resp = call(&relay_url, &alice, &c.faucet_pubkey, "get_info", json!({})).await?;
        let listed: Vec<String> = resp
            .pointer("/result/methods")
            .context("get_info carries no methods")?
            .as_array()
            .context("methods is not an array")?
            .iter()
            .filter_map(|m| m.as_str().map(String::from))
            .collect();
        // Generated by #[nostr_ln::service] from the impl block, so it
        // cannot disagree with what the faucet answers — and it names no
        // Lightning method, because the faucet has no Lightning wallet.
        anyhow::ensure!(
            listed == vec!["get_balance".to_string(), "get_info".to_string(), "pay_onchain".to_string()],
            "{}: advertises {listed:?}",
            c.label
        );
        anyhow::ensure!(
            !listed.iter().any(|m| m.contains("invoice")),
            "{}: a faucet with no Lightning wallet must not advertise one",
            c.label
        );
        tracing::info!("  {}: methods = {listed:?}", c.label);
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
    owner: &Keys,
) -> Result<(PublicKey, ManagedChild)> {
    let keys = Keys::generate();
    let dir = out.join(label);
    std::fs::create_dir_all(&dir)?;
    let cfg_path = dir.join("faucet.toml");

    // A window in seconds and a cap in coins, not weeks — the reason both
    // are configuration rather than constants.
    //
    // No `[policy]` section: per-key limits are grants now. What is left is
    // the one limit a grant cannot express, because it is a property of the
    // faucet rather than of anyone asking.
    std::fs::write(
        &cfg_path,
        faucet_toml(
            relay_url,
            &keys.secret_key().to_secret_hex(),
            &owner.public_key(),
            node.rpc_port(),
            node.rpc_user(),
            node.rpc_password(),
            label,
            ONE_COIN_SAT * 5,
            WINDOW_SECS,
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
