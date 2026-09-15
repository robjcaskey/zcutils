# Offline FIPS compilation

The canonical build prepares its inputs online, compiles the native AWS-LC
provider and all application executables without network access, then assembles
and signs the release. Input acquisition, GitHub orchestration, and publication
still use the network. This is not an air-gapped end-to-end workflow.

## Compilation

`scripts/fips-reproducible-provider.py` invokes the prescribed AWS-LC procedure
inside a new network namespace containing only loopback. An unprivileged
runner creates it through a user namespace; passwordless sudo is unnecessary. The source archive is
verified before execution. The resulting provider package is the input to the
application build; Cargo does not recompile the native module.

`Dockerfile.fips` prepares OS packages, the Rust toolchain, and locked source
dependencies in `build-inputs`. Cargo's download cache seeds a complete copy of
those dependencies in `/opt/cargo-inputs`; compilation does not mount that cache
or reuse compiled outputs. `offline-build` starts with an empty target directory
and runs Cargo with `--frozen` under `RUN --network=none`. Its manifest records
the source/compiler identity, provider hash, observed network interfaces, and
SHA-256 of every shipped executable.

The final image takes its executables from `offline-build`. Before publication,
the image tooling extracts the actual packaged executables and verifies their
hashes against `offline-build.json`. This guards selection of the offline
outputs; it does not perform an online rebuild.

Tests and acceptance-report collection happen after compilation. SBOM generation,
signing, and publication happen after image assembly. Workflow files and
acceptance collectors needed to document the build are copied into the assembly
stage, rather than into the compiler's input stage.

## Optional comparison experiment

Normal workflow dispatches set `compare_online=false`: they do not compile an
online reference or run the same-machine comparison. For an explicit experiment,
set `compare_online=true`, or run the local workflow helper with
`--fips-build --compare-online`.

In that experiment, the native runner rebuilds the provider from clean source
at the same absolute path with networking enabled and disabled, then compares
the module object, archive, tool, and identity probe. The application Dockerfile
selects the explicit `reproducibility-check` stage: two clean compiler branches
use the same prepared inputs, and every shipped executable must have matching
SHA-256 hashes. A mismatch stops the build. Only offline outputs are selected.

These experiments establish repeatability on one builder. Reproducibility across
machines must be assessed by rebuilding on separate machines with the same
source, tools, provider procedure, and input versions and comparing the actual
outputs. Preserve the release's offline manifest and native artifact hashes for
that comparison. Per-run receipts, signatures, and OCI metadata are not expected
to match byte for byte; matching executable hashes must not be described as
matching complete OCI image digests.

After downloading and verifying two releases and their launch records, compare
independent workers with:

```sh
python3 scripts/fips-compare-builds.py \
  --first /path/to/first/artifact --first-launch /path/to/first/launch.json \
  --second /path/to/second/artifact --second-launch /path/to/second/launch.json \
  --report independent-build-comparison.json
```

The tool requires distinct worker instance IDs, verifies the actual packaged
files against the offline manifests, and compares source/tool identity and
native-provider outputs. It does not authenticate the supplied launch records;
verify the downloaded artifacts and their origin before running it.
