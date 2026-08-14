# Ad-hoc ENA low-latency helper validation

Run: `zcutils-nicfix-adhoc-c8gn48-20260809T230146Z`

- One dual-EFA/ENA `c8gn.48xlarge` instance, launched and terminated through `scripts/ec2_perf_spot.py` in placement group `up-zcutils-adhoc-us-east-2c`.
- Both ENA interfaces were discovered automatically: `ens68` on PCI `0000:47:00.0` and `ens146` on PCI `0000:9b:00.0`.
- Before apply, both interfaces reported adaptive RX on, `rx-usecs=20`, and `tx-usecs=64`.
- After apply, both reported adaptive RX off, `rx-usecs=0`, and `tx-usecs=0`.
- A separate `verify` invocation passed for both interfaces and recorded the utility run ID and node index.
- The helper's non-ad-hoc safety refusal, shell syntax, representative harness failure gate, and `git diff --check` passed locally.
- The instance was terminated and its elastic IP was released immediately after validation.
