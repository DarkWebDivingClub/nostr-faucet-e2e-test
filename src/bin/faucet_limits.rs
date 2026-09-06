//! The limits, and who may change them.
//!
//! `faucet_funds_alice` proves the faucet pays. This proves it stops, and
//! that only the owner can talk it out of stopping.
//!
//! Four things that unit tests cannot show, because they are about the wire
//! and the process rather than about arithmetic:
//!
//! - the **total cap** refuses a key that is inside its own quota, which is
//!   the control that actually bounds a script, since new keys are free;
//! - a **Lightning method** is refused as not implemented, matching what
//!   `get_info` advertises — a faucet with no Lightning wallet says so;
//! - a grant from a key that is **not an owner** changes nothing, which is
//!   [dln-node#1](https://github.com/DarkWebDivingClub/dln-node/issues/1);
//! - the owner widens and revokes **live, with no restart**, because a
//!   grant is an event rather than a config file.
//!
//! The last two replaced `pause`, `resume` and `set_policy`, which this
//! faucet invented and mission 13.4 removed. There is no global pause any
//! more: the open-faucet policy is an `OTHERS` grant, and revoking it is
//! what closing the faucet means.

use std::path::PathBuf;

use anyhow::{Context, Result};
use nostr_faucet_e2e_test::*;
use nostr_sdk::prelude::*;
use serde_json::json;

use dln_e2e_harness::bitcoind::BitcoindHarness;
use dln_e2e_harness::process::ManagedChild;
use dln_e2e_harness::{relay, util};

const PREMINE: u64 = 110;
const ONE_COIN_SAT: u64 = 100_000_000;
const WINDOW_SECS: u64 = 120;
/// Two coins across everyone, so a third asker hits it while still inside
/// their own untouched one-coin quota.
const TOTAL_CAP_SAT: u64 = 200_000_000;

/// One coin per asker, per window. Written as a grant, which is where all
/// per-key policy lives now.
fn one_coin_grant() -> String {
    format!(
        r#"{{"methods":{{"OTHERS":{{}}}},"quota":{{"amount":{ONE_COIN_SAT},"per_secs":{WINDOW_SECS},"max_capacity":{ONE_COIN_SAT}}}}}"#
    )
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,testcontainers=warn")),
        )
        .init();
    match run().await {
        Ok(()) => println!("\n=== PASS ==="),
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<()> {
    let out = PathBuf::from(
        std::env::var("OUTPUT_DIR").unwrap_or_else(|_| util::unique_tmp_dir("faucet-limits")),
    );
    std::fs::create_dir_all(&out)?;

    let faucet_bin = faucet_binary()?;
    let (_relay_container, relay_url) = relay::start_relay().await;

    let node = BitcoindHarness::start().await;
    let miner_addr = node.get_new_address().await;
    node.mine_blocks(PREMINE, &miner_addr).await;
    node.create_wallet("treasury").await;

    let owner = Keys::generate();
    let (faucet_pubkey, _child) =
        start_faucet(&node, &relay_url, &faucet_bin, &out, &owner).await?;

    // ── The total cap ────────────────────────────────────────────────────
    // Two keys take a coin each, exhausting a two-coin cap. A third key has
    // taken nothing and is well inside its own quota, and must still be
    // refused — that is the whole point of a cap as well as a quota, and it
    // is the one limit that is still the faucet's rather than a grant's.
    tracing::info!("Step 1: two keys exhaust the total cap");
    for i in 1..=2 {
        let asker = Keys::generate();
        publish_grant(&owner, &faucet_pubkey, &asker.public_key(), &one_coin_grant(), &relay_url)
            .await?;
        let addr = treasury_address(&node).await?;
        let resp = ask(&relay_url, &asker, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
        anyhow::ensure!(refusal(&resp).is_none(), "asker {i} was refused: {:?}", refusal(&resp));
        node.mine_blocks(1, &miner_addr).await;
    }

    tracing::info!("Step 2: a third key, inside its own quota, is refused by the cap");
    let fresh = Keys::generate();
    publish_grant(&owner, &faucet_pubkey, &fresh.public_key(), &one_coin_grant(), &relay_url)
        .await?;
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &fresh, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let message = resp_refusal(&resp, "a key inside its own quota was paid past the total cap")?;
    anyhow::ensure!(
        message.contains("total cap"),
        "refused, but not by the cap: {message}"
    );
    tracing::info!("  refused — {message}");

    // ── A Lightning method, which this faucet does not implement ─────────
    tracing::info!("Step 3: a Lightning method is refused as not implemented");
    let resp = call(
        &relay_url,
        &fresh,
        &faucet_pubkey,
        "pay_invoice",
        json!({"invoice": "lnbcrt1p000000"}),
    )
    .await?;
    let code = resp
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .context("pay_invoice was not refused by an on-chain-only faucet")?;
    anyhow::ensure!(
        code == "NOT_IMPLEMENTED",
        "refused as {code}, expected NOT_IMPLEMENTED — the faucet implements \
         no Lightning method and its info event says so"
    );
    tracing::info!("  refused — NOT_IMPLEMENTED");

    // ── Who may change the limits ────────────────────────────────────────
    // This replaced `pause` and `set_policy`. There is no control method to
    // guard any more; there is a grant, and it counts only if an owner
    // signed it.
    tracing::info!("Step 4: a grant from a key that is not an owner changes nothing");
    //
    // Asserted on the **error code**, not merely on being refused. The cap
    // is exhausted by now, so a key that the impostor's grant *did*
    // authorize would still be refused — by the cap. Only the code tells
    // the two apart:
    //
    //   UNAUTHORIZED    the grant was ignored, which is correct
    //   QUOTA_EXCEEDED  the grant took effect and the cap caught it, which
    //                   is dln-node#1
    //
    // The key is new and the owner has never granted it, so it has no
    // authorization of its own to fall back on.
    let victim = Keys::generate();
    let impostor = Keys::generate();
    let greedy = format!(
        r#"{{"methods":{{"OTHERS":{{}}}},"quota":{{"amount":{},"per_secs":1,"max_capacity":{}}}}}"#,
        ONE_COIN_SAT * 100,
        ONE_COIN_SAT * 100
    );
    publish_grant(&impostor, &faucet_pubkey, &victim.public_key(), &greedy, &relay_url).await?;
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &victim, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let code = resp
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .context("a grant signed by a non-owner took effect — dln-node#1")?;
    anyhow::ensure!(
        code == "UNAUTHORIZED",
        "refused as {code}, expected UNAUTHORIZED — anything else means the \
         impostor's grant authorized this key and something later refused it"
    );
    tracing::info!("  refused as UNAUTHORIZED — only an owner's grant counts");

    // ── The owner, live ──────────────────────────────────────────────────
    tracing::info!("Step 5: the owner revokes, live, with no restart");
    let alice = Keys::generate();
    publish_grant(&owner, &faucet_pubkey, &alice.public_key(), &one_coin_grant(), &relay_url)
        .await?;
    publish_grant(&owner, &faucet_pubkey, &alice.public_key(), "{}", &relay_url).await?;
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &alice, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let code = resp
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .context("an empty grant did not revoke")?;
    anyhow::ensure!(
        code == "UNAUTHORIZED" || code == "RESTRICTED",
        "refused as {code} — a revoked key must be refused for the revocation, \
         not by a limit that would have refused it anyway"
    );
    tracing::info!("  refused as {code} — for the revocation, not for a limit");
    tracing::info!("  an empty grant is how a faucet closes; there is no pause method");

    // ── OTHERS, which is what makes a faucet open ────────────────────────
    tracing::info!("Step 6: OTHERS opens the faucet; an explicit empty grant still denies");
    publish_grant_for(&owner, &faucet_pubkey, "OTHERS", &one_coin_grant(), &relay_url).await?;

    // The cap is exhausted by now, so nobody gets paid here — which is
    // fine, because what is being tested is *authorization*, and the error
    // code separates it cleanly from the cap.
    let stranger = Keys::generate();
    let addr = treasury_address(&node).await?;
    let resp = ask(&relay_url, &stranger, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let code = resp
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .unwrap_or("<paid>");
    anyhow::ensure!(
        code != "UNAUTHORIZED" && code != "RESTRICTED",
        "a key with no grant of its own was refused as {code} while an OTHERS \
         grant was in force — OTHERS is what makes the faucet open"
    );
    tracing::info!("  a stranger is authorized by OTHERS (refused as {code}, not for authorization)");

    // An explicit entry beats OTHERS, which is what makes a deny-list
    // possible on top of an open policy. Alice was revoked in step 5 and
    // OTHERS has since been published; she must still be refused.
    let resp = ask(&relay_url, &alice, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let code = resp
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .unwrap_or("<paid>");
    anyhow::ensure!(
        code == "UNAUTHORIZED" || code == "RESTRICTED",
        "a key with an explicit empty grant was let through by OTHERS \
         (refused as {code}) — an explicit entry must beat the default, or \
         a deny-list is impossible on an open faucet"
    );
    tracing::info!("  and an explicit empty grant still denies — refused as {code}");

    // ── `rate`, spelled as the specification spells it ───────────────────
    // dln-node#2 is this field named `access_rate`, which makes a
    // conforming grant deserialise to **no limit at all** — silent, and in
    // the permissive direction. So the test is that a grant written against
    // the specification actually limits.
    //
    // Rate is step 5a of the pipeline, before the cost is prepared and
    // before the cap is checked, so this reads clearly even with the cap
    // exhausted: the first ask is refused by the cap, the second by the
    // rate.
    tracing::info!("Step 7: a grant using `rate` limits, rather than silently not");
    let hasty = Keys::generate();
    publish_grant(
        &owner,
        &faucet_pubkey,
        &hasty.public_key(),
        &format!(
            r#"{{"methods":{{"pay_onchain":{{"rate":{{"amount":1,"per_secs":600,"max_capacity":1}}}}}},"quota":{{"amount":{ONE_COIN_SAT},"per_secs":{WINDOW_SECS},"max_capacity":{ONE_COIN_SAT}}}}}"#
        ),
        &relay_url,
    )
    .await?;

    let first = ask(&relay_url, &hasty, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let first_code =
        first.pointer("/error/code").and_then(|c| c.as_str()).unwrap_or("<paid>");
    anyhow::ensure!(
        first_code != "RATE_LIMITED",
        "the first request of a one-per-window rate was already rate limited"
    );

    let second = ask(&relay_url, &hasty, &faucet_pubkey, &addr, ONE_COIN_SAT).await?;
    let second_code = second
        .pointer("/error/code")
        .and_then(|c| c.as_str())
        .unwrap_or("<paid>");
    anyhow::ensure!(
        second_code == "RATE_LIMITED",
        "a second request inside a one-per-window `rate` was refused as \
         {second_code} — a grant that names `rate` must limit. Deserialising \
         it to no limit at all is dln-node#2"
    );
    tracing::info!("  second request refused as RATE_LIMITED — `rate` is read, not ignored");

    Ok(())
}

/// The refusal message, or a failure carrying `why`.
fn resp_refusal(v: &serde_json::Value, why: &str) -> Result<String> {
    refusal(v).context(why.to_string())
}

fn faucet_binary() -> Result<String> {
    if let Ok(p) = std::env::var("NOSTR_FAUCET_BINARY") {
        return Ok(p);
    }
    let repo = std::env::var("NOSTR_FAUCET_REPO")
        .unwrap_or_else(|_| format!("{}/git/nostr-faucet", std::env::var("HOME").unwrap()));
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "nostr-faucet"])
        .current_dir(&repo)
        .status()?;
    anyhow::ensure!(status.success(), "building nostr-faucet failed");
    Ok(format!("{repo}/target/debug/nostr-faucet"))
}

async fn start_faucet(
    node: &BitcoindHarness,
    relay_url: &str,
    binary: &str,
    out: &std::path::Path,
    owner: &Keys,
) -> Result<(PublicKey, ManagedChild)> {
    let keys = Keys::generate();
    std::fs::create_dir_all(out)?;
    let cfg_path = out.join("faucet.toml");
    std::fs::write(
        &cfg_path,
        faucet_toml(
            relay_url,
            &keys.secret_key().to_secret_hex(),
            &owner.public_key(),
            node.rpc_port(),
            node.rpc_user(),
            node.rpc_password(),
            "limits.regtest",
            TOTAL_CAP_SAT,
            WINDOW_SECS,
        ),
    )?;
    let child = ManagedChild::spawn("faucet", binary, &[cfg_path.to_str().unwrap()], out)?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    Ok((keys.public_key(), child))
}

async fn treasury_address(node: &BitcoindHarness) -> Result<String> {
    node.rpc_wallet("treasury", "getnewaddress", json!(["", "bech32"]))
        .await
        .map_err(|e| anyhow::anyhow!("treasury getnewaddress: {e}"))?
        .as_str()
        .map(String::from)
        .context("getnewaddress did not return a string")
}
