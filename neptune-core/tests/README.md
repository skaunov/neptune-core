<https://doc.rust-lang.org/rust-by-example/testing/integration_testing.html>

## `--nocapture` behavior

Since each test is in a separate crate, `tracing` events do not get displayed, even with `cargo test --nocapture`.  If `#[traced_test]` is used only the events from the test itself appear, none from neptune_cash.

To workaround this limitation, use:

`NOCAPTURE=1 cargo test --nocapture`

## regtest mode

Integration tests (should) typically make use of the regtest network mode.  This mode uses mock proofs and allows transactions and blocks to be created quickly in a deterministic fashion.

see <https://github.com/Neptune-Crypto/neptune-core/issues/539>

Some APIs specific to regtest mode exist in src/api/regtest.

## genesis_node

The `GenesisNode` type in <./common/genesis_node.rs> facilitates the creation of a cluster of N connected peers.  Example usage.

```rust
// start 2 peer network of alice and bob.
let [(mut alice_gsl, _jh1), (mut bob_gsl, _jh2)] =
    GenesisNode::start_n_nodes(2).await?.try_into().unwrap();
```

In this example `alice_gsl` and `bob_gsl` represent an instance of `GlobalStateLock` for Alice's node and Bob's node respectively.

The unused `_jh1` and `_jh2` are Tokio `JoinHandle` for waiting on the main application loop of each node.

See existing tests for further usage.

## API layer

It is hoped/intended that these tests will primarily make use of the public interface in <src/api>.

When the <src/api> is insufficient, that is a good indicator it needs to be extended and improved.

## Style.

It is hoped these test can also serve a dual-purpose as example usage.  To that end, they should be as clear and readable as possible.

All tests should have a doc-comment describing the purpose and basic steps of the test.

### conciseness  
Let's keep these tests as short and sweet as we can.
Long tests are hard to read and comprehend and they make the build-test cycle slower.
### simple logic
Use straight code flow as much as possible. Aim for  general code coverage and example usage over complicated testing of every possible scenario.
### conventions
Please study the existing tests and follow the same style and convention when adding your own or modifying.

## Not the only game in town.
Keep in mind that anyone can create separate crate(s) that use the same public APIs. So for any long and complicated scenarios it is likely better to place them in one or more companion crates dedicated to integration tests.

## Unchanged Consensus Rules
To allow for testing of the PoW algorithm, the data structure of blocks is different under the test flag than it is in production. Otherwise running the test would require at least 40GB RAM and take dozens of minutes more than it does now. So tests that the genesis blocks have not changed must live outside of the `cfg(test)` flag. For this reasons, such tests are added as integration tests here. This allows for the comparison of the hash of genesis blocks to that presented by various block explorers, giving the developer confidence that they didn't accidently change it.
