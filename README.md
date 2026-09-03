# nostr-faucet-e2e

End-to-end scenarios for [`nostr-faucet`](https://github.com/DarkWebDivingClub/nostr-faucet).

## Why regtest

**The faucet cannot tell regtest from signet.** It holds RPC to a
`bitcoind` and pays out of a wallet; the chain type is invisible to it
except for block timing. So running against locally spun regtest chains is
a faithful test of everything the faucet does, and confirmations are mined
on demand rather than waited for — seconds instead of minutes.

Shape: one node per chain, two wallets each. `testwallet` is the miner,
which the faucet spends from; `treasury` is what Alice fills. Cheaper than
four nodes and it covers the whole path. What it does not exercise is
network separation between Alice's node and the faucet's, which is not what
these scenarios are for.

## Scenarios

| | |
|---|---|
| `faucet_funds_alice` | Alice asks both faucets for a coin and gets both; a second ask inside the window is refused with a reason; each faucet advertises `onchain` and nothing else |
| `faucet_limits` | the global cap refuses a key inside its own quota; a Lightning URI is refused for a stated reason; only the control key can pause or change policy, and it takes effect without a restart |

```
cargo run --bin faucet_funds_alice
cargo run --bin faucet_limits
```

The faucet binary is built from `~/git/nostr-faucet` unless
`NOSTR_FAUCET_BINARY` or `NOSTR_FAUCET_REPO` says otherwise.

## Note on the harness

`dln-e2e-harness` comes from `diamond-x-e2e` as a git dependency, the same
way `dln-node-e2e` and `dln-node-knots-e2e` consume it. **A git-dependency
consumer does not see harness changes until its own lockfile updates** —
which is how, after Mission 08, scenarios were reported as fixed while
still building against the old code. Anything this suite needs from the
harness lands in `diamond-x-e2e` master first, and the lockfile here is
updated deliberately.
