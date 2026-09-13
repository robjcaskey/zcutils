# Optional persistent runner cache

Deployed in AWS account `968134102381` on 2026-09-13:

* volume `vol-0bd047325c3a96e98` in `us-east-1a`
* policy `arn:aws:iam::968134102381:policy/zcutils-build-cache-Rob-J-Caskey-attach`
* policy attachment to `zcutils-fips-runner-controller`

GitHub repository variables contain the exact volume and zone. First-use empty
volume formatting is enabled for the next certificate-profile build; after
that build it must be returned to `false`. Squid remains disabled by default.

This separate Terraform module defines one encrypted 20 GiB gp3 EBS volume in
one explicit availability zone and an IAM policy for attaching that exact
volume only to instances carrying the configured `RunnerPool` tag. The policy
is attached to the one existing role named by `controller_role_name`. It creates
no EC2 instances, launch templates, network interfaces, public addresses, or
instance-type/AMI restrictions. The main runner controller remains responsible
for allowing trusted workflows to select any reviewed concrete AMI and instance
type in its dedicated network and for requiring its existing cleanup tags.
The attach policy independently requires the runner's `terminate` keepalive
mode and a bounded-format `ExpiresAt` tag; it does not relax launch permission.

The volume is static, same-AZ, and single-writer. gp3 does not support EBS
Multi-Attach, `multi_attach_enabled` is explicitly false, and the bootstrap
rejects anything except one attachment to the current instance. EC2 detaches
the non-root static volume during instance termination; controller cleanup
waits for the volume to return to `available` before removing its independent
termination schedule. `prevent_destroy` requires a deliberate source review
before deleting the persistent cache.

The volume incurs EBS storage cost. An administrator can review future changes
with:

```sh
terraform init
terraform plan -var aws_account_id=968134102381 \
  -var availability_zone=us-east-1a \
  -var controller_role_name=zcutils-fips-runner-controller
```

Applying the plan creates the volume, IAM policy, and role-policy attachment,
but does not attach the volume or launch an instance. Those external steps
require separate approval. Store the exact `volume_id` and `availability_zone`
outputs as `AWS_FIPS_RUNNER_CACHE_VOLUME_ID` and
`AWS_FIPS_RUNNER_CACHE_AVAILABILITY_ZONE`. Only the FIPS build label consumes
them; an empty volume ID disables the cache. A brand-new volume has no
filesystem. Set `AWS_FIPS_RUNNER_CACHE_ALLOW_FORMAT_EMPTY=true` for exactly the
reviewed first build, then immediately return it to `false`; root formats only
the exact controller-validated empty volume. After an ephemeral AL2023 runner
has the volume attached, check it
before any formatting or mount action:

```sh
sudo scripts/configure-runner-build-cache.sh --check \
  --volume-id vol-0123456789abcdef0 \
  --device /dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol0123456789abcdef0
```

`--apply --allow-format-empty` is deliberately required for first use. It
creates persistent Cargo registry/git download directories and a DNF package
cache. It deliberately does not persist `CARGO_TARGET_DIR`: compiled Cargo
outputs on this unsigned disk are not authenticated. Signed, digest-pinned OCI
BuildKit cache is the only reusable compiled/layer cache. Run DNF through
`zcutils-dnf-cache`; Cargo.lock checksums and signed RPM repository metadata
remain the content-integrity authorities.

`--enable-squid` optionally installs Squid with
`config/squid-runner-build-cache.conf`. It listens only on `127.0.0.1:3128` and
can cache ordinary cacheable HTTP responses. HTTPS uses opaque CONNECT tunnels:
there is no TLS interception and CONNECT content is not cached. Squid never
decides whether a Docker BuildKit cache is trusted; the digest and cosign checks
in `scripts/zc-image-attest.py` remain mandatory.

The automated root bootstrap provides the same loopback-only service when
`AWS_FIPS_RUNNER_CACHE_ENABLE_SQUID=true`; it defaults to `false` because the
current AWS-LC, Cargo, and repository downloads use HTTPS, whose CONNECT
payload Squid cannot content-cache without forbidden TLS interception. Enabling
it sets loopback `HTTP_PROXY`/`http_proxy` for the runner service and exempts
localhost and IMDS. It does not set `HTTPS_PROXY` or intercept TLS.

Certificate-profile workers always run the digest-pinned Docker Distribution
registry on `127.0.0.1:5000`, backed by the volume's `registry/` directory.
BuildKit runs in host-network mode using
`config/buildkit-loopback-registry.toml`, so the registry is reachable from the
builder container but never exposed on the instance's public interface. Cache
manifests are KMS-signed and builder-identity-bound before a later build may
import them. Docker Hub receives only the final FIPS-aspiring runtime image and
its signature.
