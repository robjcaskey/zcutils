# GitHub ephemeral EC2 runner permissions

Terraform creates the identities and empty network that a GitHub Actions job
needs to launch a temporary EC2 worker and schedule its termination. It creates
**no EC2 instances, EBS volumes, public IP allocations, or per-job schedules**.

The stack creates a separate FIPS runner VPC, GitHub OIDC provider, controller
role, restricted Scheduler execution role, empty schedule group, public subnet,
internet gateway, route table, and outbound-only security group. These resources have no
standing hourly charge. There is no NAT gateway, Elastic IP, load balancer,
interface endpoint, or always-running controller. Terraform state is local and
ignored by Git; retain it securely for subsequent updates or destruction.

Workers use automatically assigned public IPv4 addresses. While a worker exists,
EC2/RHEL, EBS, public IPv4, and applicable data transfer charges apply. Public IPv4
currently costs $0.005 per address-hour. Termination releases the automatic IP;
the runtime must set `DeleteOnTermination` on its volumes and network interface.
Scheduler charges are invocation-based, with a published free allowance; an
empty schedule group does not invoke anything. GitHub Actions minutes and
artifact retention are billed separately under the repository's plan.
See [AWS VPC pricing](https://aws.amazon.com/vpc/pricing/) and
[EventBridge pricing](https://aws.amazon.com/eventbridge/pricing/).

## Authentication

1. A trusted workflow job requests a GitHub OIDC token with `id-token: write`.
2. `aws-actions/configure-aws-credentials` calls
   `sts:AssumeRoleWithWebIdentity` with that token and the controller role ARN.
3. AWS verifies GitHub's signature, the `sts.amazonaws.com` audience, and the
   exact subject in the role's trust policy. It returns temporary credentials.

The example trusts exactly
`repo:robjcaskey/zcutils:ref:refs/heads/main`, verified against this repository's
OIDC customization API. No AWS access key is required. The ARN is a normal
GitHub repository variable, not a secret. The job must run on `main` without a
GitHub environment to match that subject. A different branch, an ordinary PR
merge job, or another repository cannot use it.

This is a branch-level trust boundary: **any job with that subject and OIDC
permission can assume the role**, not just a particular workflow filename.
Only run reviewed workflow code in privileged jobs. Do not run untrusted PR
code in a `pull_request_target` job; such jobs can carry the base branch's
identity. If adopting a GitHub environment or immutable repository-ID subjects,
update the exact trust and any environment protection rules accordingly.

```bash
gh api repos/robjcaskey/zcutils/actions/oidc/customization/sub
```

See [GitHub's AWS OIDC guide](https://docs.github.com/en/actions/how-tos/secure-your-work/security-harden-deployments/oidc-in-aws).
The [authentication workflow example](examples/auth-oidc.yml) only reads the AWS
caller identity. It is outside `.github/workflows` and will not run until added
there. Registering an actual self-hosted GitHub runner requires separate GitHub
runner-registration credentials, such as a repository-scoped GitHub App; AWS
OIDC does not grant GitHub API permissions.

The [end-to-end smoke workflow](../../../.github/workflows/fips-ec2-runner-smoke.yml)
can now launch one worker and verify a hello-world artifact. Run
`python3 scripts/test-github-ec2-runner.py` from the repository root with an
administrative `gh` login. It mints a one-job runner configuration, dispatches
the workflow, downloads the artifacts, and removes its temporary GitHub secret.
See [the smoke-test guide](../../../docs/github-ec2-runner-smoke.md).

## Provision

Use the administrative AWS profile only for this bootstrap. The GitHub role has
no permission to create/update IAM policies, the network, or its schedule group.

```bash
cd zccusan/deploy/github-ec2-runner
export AWS_PROFILE=slopmud-breakglass
aws sts get-caller-identity
terraform init
terraform plan -var-file=terraform.tfvars.example -out=bootstrap.tfplan
terraform show bootstrap.tfplan
terraform apply bootstrap.tfplan
```

The provider enforces the account ID in the configuration. The role permits a
trusted workflow to select any concrete AMI and instance type in `us-east-1`.
Each workflow must pin its intended values, and the worker must verify the
actual result through IMDS, DMI and the operating-system release before work.

The `10.84.0.0/16` VPC and `10.84.0.0/24` subnet are separate from the account's
default and `AdhocDeployment` VPCs. This stack does not attach gateways to,
change routes in, or peer with those networks. Preserve this separation: opening
internet egress for bulk HPC experiments could incur substantial transfer costs.

The documented `/home/rob/spot-helper/ec2_perf_spot.py` discovers subnets using
`default-for-az=true` and `map-public-ip-on-launch=true`. This runner subnet is
**not** a default subnet, so the helper will not select it automatically. Its
`launch` command also requires an explicit subnet ID. The helper does not check
internet route health when recommending those default subnets. The performance
policy in `/home/rob/docs/performance-cloud-compute.md` requires same-subnet/AZ
private addresses for bulk traffic; public management addresses do not establish
that an experiment's data path is free. Do not pass this FIPS subnet to HPC runs.

To authorize an already working dedicated public subnet without creating any
network resources, set `create_public_network = false` and provide explicit
subnet and security group allowlists. This option grants use of those resources
without modifying them. A public IP still requires a working internet gateway
route to provide internet connectivity.

If this account already has a GitHub OIDC provider, set
`existing_github_oidc_provider_arn` instead of creating a second provider.
When an existing shared provider is used, this stack does not manage it.

Store these non-secret repository variables:

```bash
terraform output -raw controller_role_arn |
  gh variable set AWS_FIPS_RUNNER_ROLE_ARN --repo robjcaskey/zcutils
gh variable set AWS_FIPS_RUNNER_REGION --repo robjcaskey/zcutils --body us-east-1
gh variable set AWS_FIPS_RUNNER_ACCOUNT_ID --repo robjcaskey/zcutils --body 968134102381
terraform output -json runner_configuration |
  gh variable set AWS_FIPS_RUNNER_CONFIG --repo robjcaskey/zcutils
```

## Runtime launch and cleanup contract

The controller may launch a concrete AMI/size selected by trusted workflow code
in the allowed network. It must request IMDSv2, default tenancy, on-demand instances, encrypted gp3 volumes
of at most `max_volume_gib`, at most 3,000 IOPS and 125 MiB/s per volume, and all
five tags below on **instances, volumes, and network interfaces**. Imported SSH
public keys, if used, also need those tags and a name starting with the pool
prefix. There is no permission to retag existing resources, attach an instance
profile, reserve an Elastic IP, or change security group rules.

| Tag | Example value |
| --- | --- |
| `RunnerPool` | `zcutils-fips-runner` |
| `RunId` | `zcutils-fips-runner-123456789-1` |
| `ExpiresAt` | `2026-09-13T04:45:00Z` |
| `adhocKeepaliveModeAction` | `terminate` (required exact value) |
| `adhocKeepalive` | `2026-09-13T04:45:00Z` (must equal `ExpiresAt`) |

The two `adhocKeepalive*` tags enroll the instance in the existing account-wide
ad hoc sweeper. IAM requires them in `RunInstances`, so creation is denied if
either is missing, the action is `stop`, or the expiry differs from `ExpiresAt`.
Tagging permission also enforces the exact tag-key casing that the sweeper reads.
The role cannot remove these tags or extend their values after creation.
This uses [EC2 tag-on-create authorization](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/supported-iam-actions-tagging.html),
including an IAM policy variable to compare the two expiry values.

Use `MinCount = MaxCount = 1`, a unique client token and run name, and a bounded
deadline between 10 and 45 minutes. Build all tags from the same UTC deadline:

```python
expiry = deadline.strftime("%Y-%m-%dT%H:%M:%SZ")
tag_values = {
    **config["required_tag_values"],
    "RunId": run_id,
    "Name": run_id,
    **{key: expiry for key in config["expiry_tag_keys"]},
}
tags = [{"Key": key, "Value": value} for key, value in tag_values.items()]
```

In the EC2 `RunInstances` request:

```python
NetworkInterfaces=[{
    "DeviceIndex": 0,
    "SubnetId": config["allowed_subnet_ids"][0],
    "Groups": config["allowed_security_group_ids"],
    "AssociatePublicIpAddress": True,
    "DeleteOnTermination": True,
}]
BlockDeviceMappings=[{
    "DeviceName": "/dev/sda1",  # Verify the selected AMI's root device name.
    "Ebs": {
        "VolumeType": "gp3", "VolumeSize": 32,
        "Iops": 3000, "Throughput": 125, "Encrypted": True,
        "DeleteOnTermination": True,
    },
}]
MetadataOptions={"HttpTokens": "required", "HttpEndpoint": "enabled"}
InstanceInitiatedShutdownBehavior="terminate"
TagSpecifications=[
    {"ResourceType": kind, "Tags": tags}
    for kind in ("instance", "volume", "network-interface")
]
```

Immediately after obtaining the instance ID, create an independent AWS
termination schedule. Do not queue the build until `GetSchedule` verifies its
instance ID, role, deadline, enabled state, and target. If scheduling fails,
terminate the instance immediately and fail the launch job. Use the same
deadline that was computed before launch, not a new deadline after bootstrap.

```python
scheduler.create_schedule(
    Name=run_id,
    GroupName=config["schedule_group_name"],
    ClientToken=run_id,
    ScheduleExpression=f"at({deadline.strftime('%Y-%m-%dT%H:%M:%S')})",
    ScheduleExpressionTimezone="UTC",
    FlexibleTimeWindow={"Mode": "OFF"},
    State="ENABLED",
    ActionAfterCompletion="DELETE",
    Target={
        "Arn": config["termination_target_arn"],
        "RoleArn": config["termination_role_arn"],
        "Input": json.dumps({"InstanceIds": [instance_id]}),
        "RetryPolicy": {
            "MaximumEventAgeInSeconds": 900,
            "MaximumRetryAttempts": 5,
        },
    },
)
```

The schedule calls EC2 directly using a role that can only terminate instances
tagged with this pool in this account/region. Its trust is scoped to the schedule
**group** ARN, as required by
[AWS Scheduler's guidance](https://docs.aws.amazon.com/scheduler/latest/UserGuide/cross-service-confused-deputy-prevention.html).
The controller can pass only that role, and only to Scheduler. It can create,
read, and delete schedules with its run prefix inside this group.

On normal completion or failure, an `always()` cleanup job should terminate the
instance, wait for termination, and then delete the schedule and any per-run
key pair. Keep the deadline schedule if termination has not been confirmed.
Upload artifacts before termination. With the prescribed deletion flags there
are no retained worker disks or allocated public IPs between jobs.

IAM checks the tag format and equality of the two expiry values, not whether
the deadline is valid or within the intended maximum lifetime.
It cannot require a schedule to accompany `RunInstances`, limit concurrent
instance count, or make credential expiry terminate an instance. The workflow
must enforce those bounds. If the controller fails between launch and schedule
creation, the required ad hoc tags enroll the instance in the shared sweeper,
which scans every 20 minutes and acts on expired or malformed leases. A guest
shutdown watchdog provides another fallback. Keep the independent per-run
schedule for a more precise deadline. Scheduler also
has minute-level precision and retries, so a deadline is not a second-exact
billing cutoff. Enforcing a maximum lifetime regardless of the requested tag
value would require a separate launcher or additional reaper logic.

This stack does not enable FIPS or validate a build. The worker's boot procedure,
cryptographic module versions, approved mode, and evidence still need the checks
in [the CSI FIPS guide](../zcblock-csi/FIPS.md).

## Optional access-key fallback

Leave `enable_key_fallback = false` for OIDC. No IAM user or access key is created
in that configuration. If a key is necessary, enable the fallback and provide
`key_pgp_public_key_base64`, produced from the **binary public key** exported by
`gpg --export KEY_ID` and then base64-encoded. Do not provide a private key.

The fallback user can only call `sts:AssumeRole` for the same controller role.
Terraform requires the public encryption key before enabling this path, and
the AWS provider stores the new AWS secret as OpenPGP ciphertext rather than
plaintext state. See the
[provider's access-key documentation](https://github.com/hashicorp/terraform-provider-aws/blob/v6.60.0/website/docs/r/iam_access_key.html.markdown).
Keep the state and plans private even when secrets are encrypted.

After applying an explicitly enabled fallback, decrypt the output locally and
put it in GitHub Actions secrets without printing it. The following uses a
private temporary file and removes it on exit:

```bash
set -euo pipefail
umask 077
key_dir=$(mktemp -d)
trap 'rm -rf -- "$key_dir"' EXIT
terraform output -raw fallback_encrypted_secret |
  base64 --decode | gpg --decrypt > "$key_dir/secret"
test -s "$key_dir/secret"
gh secret set AWS_SECRET_ACCESS_KEY --repo robjcaskey/zcutils < "$key_dir/secret"
terraform output -raw fallback_access_key_id |
  gh secret set AWS_ACCESS_KEY_ID --repo robjcaskey/zcutils
```

For this path, pass the two secrets plus `role-to-assume` to
`configure-aws-credentials` and set `force-skip-oidc: true`. Rotate the key and
remove it when OIDC is available. This path adds a long-lived credential without
adding any worker permissions beyond the controller role.
