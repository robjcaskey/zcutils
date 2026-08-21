# Getting started with zccusan on Kubernetes

This repo now publishes images and Helm charts on independent pipelines.

## Add the Helm repo

```bash
helm repo add zccusan https://robjcaskey.github.io/zcutils
helm repo update
```

## Install / upgrade with an image tag

```bash
helm upgrade --install zccusan zccusan/zcblock-csi \
  --namespace zccusan \
  --create-namespace \
  --version "0.1.2" \
  --set-string image.tag="0.1.2"
```

For first install, replace `upgrade --install` with `install`.

## Select telemetry and community-survey API endpoints

Helm does not expand shell environment variables inside a values YAML file.
The repository includes reviewed community-survey values files. Pass one as a
normal Helm values file; there is no telemetry environment or stage setting:

```bash
export ZCCUSAN_HELM_VALUES_FILE="$PWD/zccusan/charts/zcblock-csi/values-community-survey-dev.yaml"
./zccusan/deploy/zcblock-csi/install-zccusan-kubernetes.sh
```

The wrapper defaults to the matching immutable `0.1.2` chart and container
image. Set `ZCCUSAN_CHART_VERSION` and `ZCCUSAN_IMAGE_TAG` together when
testing another release.

The two profiles are:

- `zccusan/charts/zcblock-csi/values-community-survey-dev.yaml`
- `zccusan/charts/zcblock-csi/values-community-survey-prod.yaml`

Both send node-local events to the in-cluster telemetry API. Only that collector
can send a community HTTPS request. It first transforms every mixed-version raw
record into `NonIdentifyingTelemetry`: installation identity is hashed, safe
cloud/region and aggregate signals are retained, and identifying or unknown
fields are omitted before bytes leave the cluster.

For a one-off independently deployed endpoint, keep the selected profile and
override its URL for both direct and collector delivery:

```bash
ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT=https://example.execute-api.us-east-1.amazonaws.com/survey \
./zccusan/deploy/zcblock-csi/install-zccusan-kubernetes.sh
```

## Image release workflow (decoupled)

The image workflow publishes Docker tags without publishing charts.

### Image tags published

- **Per-branch / push builds**
  - `main` branch push: `main`, `latest`, `sha-<7>`
  - non-main branch push: `<branch>`, `<branch>-sha-<7>`, `sha-<7>`
- **Semantic image tags**
  - pushing `v1.2.3` or `release-1.2.3` publishes:
    - `1.2.3`, `1.2`, `1`, `sha-<7>`
  - pushing `v1.2` publishes:
    - `1.2`, `1`, `sha-<7>`
  - pushing `v1` publishes:
    - `1`, `sha-<7>`
- **Scheduled builds**
  - `nightly`, `nightly-YYYYMMDD`, `sha-<7>`
- **Manual run**
  - use `workflow_dispatch`; it publishes `sha-<7>` plus branch/tag derived tags.

## Helm chart release workflow (decoupled)

Charts are only published by the chart workflow.

### Triggering a chart release

- Tag and push `chart-v0.1.2`:

```bash
git tag chart-v0.1.2
git push origin chart-v0.1.2
```

- Or run chart workflow manually with:

```bash
gh workflow run "Promote zcblock-csi RC chart" \
  -f source_chart_tag="chart-v0.1.2-rc.1" \
  -f dry_run=false
```

The workflow packages `zccusan/charts/zcblock-csi` and publishes it to the repo's GitHub Pages index.

### Chart versioning

- Chart versions are independent and typically patch-level semver (`0.1.2`).
- For deterministic deployments, pin both chart `--version` and `--set image.tag`.
- For mutable tracks in non-prod:
  - pin only chart `--version`
  - set image by mutable tag (`main`, `nightly`, branch name)

## Floating semver vs immutable tags

- **Immutable:** `sha-<7>`, full patch tags (`0.1.2`) in both image and chart
- **Mutable by design:** `main`, `latest`, `nightly`, branch names, `1`, `1.2`
- For strict reproducibility, use immutable patch tags for both image and chart.

## Verify artifact signatures

Signatures are produced in GitHub Actions for release workflow outputs.

### Verify container image signatures

Install cosign:

```bash
curl -sSfL https://github.com/sigstore/cosign/releases/latest/download/cosign-linux-amd64 -o /tmp/cosign
chmod +x /tmp/cosign
sudo mv /tmp/cosign /usr/local/bin/cosign
```

Verify a pushed image signature:

```bash
COSIGN_EXPERIMENTAL=1 cosign verify \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --certificate-identity-regexp ".*github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-images.yml.*" \
  robjcaskey/zcblock-csi:1.2.3
```

If you need a no-network workflow check, you can also verify digest first:

```bash
cosign triangulate robjcaskey/zcblock-csi:1.2.3
```

### Verify Helm chart signatures

The chart promotion workflow publishes `*.sig` and optional `*.sig.release` files next to the `.tgz` on GitHub Pages for chart artifacts.

```bash
export VERSION="0.1.0-nightly.20260819.1"
export BASE="https://robjcaskey.github.io/zcutils"
export CHART="zcblock-csi-${VERSION}.tgz"

curl -fL -o /tmp/${CHART} "${BASE}/${CHART}"
curl -fL -o /tmp/${CHART}.sig "${BASE}/${CHART}.sig"

COSIGN_EXPERIMENTAL=1 cosign verify-blob \
  --signature "/tmp/${CHART}.sig" \
  --certificate-identity-regexp ".*github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-promote-chart-rc.yml.*" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  "/tmp/${CHART}"
```

For stronger release-only checks, verify the `.sig.release` artifact after it is published by the same workflow:

```bash
curl -fL -o /tmp/${CHART}.sig.release "${BASE}/${CHART}.sig.release"
COSIGN_EXPERIMENTAL=1 cosign verify-blob \
  --signature "/tmp/${CHART}.sig.release" \
  "/tmp/${CHART}"
```

## Upgrade and rollback

```bash
helm upgrade --install zccusan zccusan/zcblock-csi \
  --version "0.1.2" \
  --namespace zccusan \
  --create-namespace \
  --set image.tag="0.1.2"
```

```bash
helm rollback zccusan 0
```
