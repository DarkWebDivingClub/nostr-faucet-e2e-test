//! Shared setup for the faucet scenarios.
//!
//! The faucet is a `nostr-ln` consumer since mission 13.4, so what these
//! scenarios have to arrange changed: a policy file became **grants**, and
//! NIP-04 became NIP-44.
//!
//! The NWC round trip is hand-rolled because `nostr-ln` has no NWC client.
//! That is mission 11.2's, and until it lands this and `nostr-faucet`'s own
//! client are two renderings of the same three fields — which is exactly
//! the thing this epic exists to stop, recorded rather than hidden.

use std::time::Duration;

use anyhow::{ensure, Result};
use nostr_ln::GRANT_KIND;
use nostr_sdk::prelude::*;
use serde_json::{json, Value};

pub const REQUEST_KIND: u16 = 23194;
pub const RESPONSE_KIND: u16 = 23195;

/// Long enough for a relay round trip.
pub const SETTLE: Duration = Duration::from_millis(600);

/// The faucet's config file.
///
/// Two sections and no policy: per-key limits are grants now, and the only
/// limit left here is the one a grant cannot express — what the faucet will
/// pay out in total.
pub fn faucet_toml(
    relay: &str,
    secret_key: &str,
    owner: &PublicKey,
    rpc_port: u16,
    rpc_user: &str,
    rpc_password: &str,
    chain_label: &str,
    total_cap_sats: u64,
    window_secs: u64,
) -> String {
    format!(
        r#"
[nostr]
relay = "{relay}"
secret_key = "{secret_key}"
owners = ["{}"]

[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = {rpc_port}
rpc_user = "{rpc_user}"
rpc_password = "{rpc_password}"
wallet = "testwallet"
chain_label = "{chain_label}"

[faucet.total_cap]
amount = {total_cap_sats}
per_secs = {window_secs}
max_capacity = {total_cap_sats}
"#,
        owner.to_hex(),
    )
}

/// Publish a grant for an asker, signed by the owner.
///
/// This is what replaced the faucet's policy file. An empty profile — `{}`
/// — is a revocation.
pub async fn publish_grant(
    owner: &Keys,
    faucet: &PublicKey,
    asker: &PublicKey,
    profile: &str,
    relay: &str,
) -> Result<()> {
    publish_grant_for(owner, faucet, &asker.to_hex(), profile, relay).await
}

/// As [`publish_grant`], but for a `d`-tag controller that is not a key.
///
/// The one that matters is the literal `OTHERS`, which is the grant applied
/// to any key with no grant of its own — an open faucet, expressed as a
/// grant rather than as a config flag.
pub async fn publish_grant_for(
    owner: &Keys,
    faucet: &PublicKey,
    controller: &str,
    profile: &str,
    relay: &str,
) -> Result<()> {
    let client = Client::builder().signer(owner.clone()).build();
    client.add_relay(relay).await?;
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let d = format!("{}:{}", faucet.to_hex(), controller);
    let event = EventBuilder::new(Kind::Custom(GRANT_KIND), profile)
        .tags([Tag::identifier(d), Tag::public_key(*faucet)])
        .sign(owner)
        .await?;
    client.send_event(&event).await?;
    tokio::time::sleep(SETTLE).await;
    client.shutdown().await;
    Ok(())
}

/// One NWC round trip, NIP-44 both ways.
///
/// It was NIP-04 until 2026-09-06, which `nostr-ln` refuses outright rather
/// than guessing at —
/// [dln-node#3](https://github.com/DarkWebDivingClub/dln-node/issues/3) is
/// the same default in another repository.
pub async fn call(
    relay: &str,
    us: &Keys,
    faucet: &PublicKey,
    method: &str,
    params: Value,
) -> Result<Value> {
    let client = Client::builder().signer(us.clone()).build();
    client.add_relay(relay).await?;
    client.connect().await;

    client
        .subscribe(
            Filter::new()
                .kind(Kind::Custom(RESPONSE_KIND))
                .pubkey(us.public_key())
                .since(Timestamp::now()),
        )
        .await?;

    let payload = json!({ "method": method, "params": params }).to_string();
    let ciphertext = us.nip44_encrypt(faucet, &payload).await?;
    let event = EventBuilder::new(Kind::Custom(REQUEST_KIND), ciphertext)
        .tag(Tag::public_key(*faucet))
        .sign(us)
        .await?;
    let request_id = event.id;
    client.send_event(&event).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        ensure!(
            !left.is_zero(),
            "no answer from the faucet in 30s — a faucet that is down and a faucet \
             that refused must not look the same, so this is a failure"
        );
        let found = client
            .fetch_events(
                Filter::new()
                    .kind(Kind::Custom(RESPONSE_KIND))
                    .pubkey(us.public_key())
                    .event(request_id),
            )
            .timeout(Duration::from_secs(2))
            .await?;
        if let Some(e) = found.first() {
            let plain = us.nip44_decrypt(&e.pubkey, &e.content).await?;
            let _ = client.disconnect().await;
            return Ok(serde_json::from_str(&plain)?);
        }
    }
}

/// Ask the faucet to pay an address.
pub async fn ask(
    relay: &str,
    us: &Keys,
    faucet: &PublicKey,
    address: &str,
    amount_sats: u64,
) -> Result<Value> {
    call(
        relay,
        us,
        faucet,
        "pay_onchain",
        json!({ "address": address, "amount_sats": amount_sats }),
    )
    .await
}

/// The `error.message` of a response, if it is one.
pub fn refusal(v: &Value) -> Option<String> {
    v.get("error")
        .filter(|e| !e.is_null())
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}
