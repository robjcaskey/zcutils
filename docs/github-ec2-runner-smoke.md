# Ephemeral EC2 runner smoke workflow

`fips-ec2-runner-smoke.yml` tests GitHub OIDC authentication to the restricted AWS
controller role, creation of one RHEL EC2 instance, one-job runner registration,
artifact upload/download, and termination. It is manually dispatched on `main`.
This is an infrastructure smoke test, not a FIPS validation claim.

Run it from a checkout with `gh` authenticated as a repository administrator:

```bash
python3 scripts/test-github-ec2-runner.py
```

The helper creates a GitHub just-in-time runner configuration and places it in a
unique temporary repository secret. It dispatches the workflow, follows the
jobs, downloads artifacts under `target/github-ec2-runner-smoke/`, verifies
`hello.txt` and its EC2 instance identity, then removes the temporary secret and
any remaining runner registration. No long-lived GitHub or AWS credential is
copied into the workflow or worker. An unattended production launcher would
need its own GitHub App or equivalent registration credential.

The workflow expects these non-secret repository variables from the Terraform
runner stack: `AWS_FIPS_RUNNER_ROLE_ARN`, `AWS_FIPS_RUNNER_ACCOUNT_ID`,
`AWS_FIPS_RUNNER_REGION`, and `AWS_FIPS_RUNNER_CONFIG`. The configuration includes
the workflow-selected AMI, dedicated subnet/security group, required ad hoc tags, and
Scheduler role/group. The controller role must trust the repository's exact
`main` branch OIDC subject.

The launch job assumes that role, creates one `m6i.large` with a 32-GiB encrypted
gp3 root volume and an automatically assigned public IP, and verifies an AWS
termination schedule for 15 minutes after launch begins. The instance is tagged
`adhocKeepaliveModeAction=terminate` and `adhocKeepalive=<deadline>` from creation
so the shared ad hoc sweeper provides another cleanup path. Its runner accepts
one job; a guest shutdown watchdog and shutdown-on-runner-exit also terminate it.

The EC2 job writes `Hello, world!` and a JSON record with its instance ID, AMI,
RHEL release, and runner identity. A GitHub-hosted job downloads and checks those
files. The final cleanup job confirms EC2 termination before removing the
per-run schedule. Root disk and network interface deletion are requested at
launch, and the automatic public IP is released with the instance.

Existing ad hoc/default VPC routing is not modified. No bulk HPC traffic is
generated. EC2/RHEL, temporary disk/IP usage, GitHub minutes/artifact retention,
and applicable transfer charges apply during this test. The persistent runner
bootstrap has no standing compute or NAT gateway charge.

Artifacts are retained on GitHub for three days. Local `report.json` identifies
the workflow run, downloaded artifacts, worker, and registration/secret cleanup.
If the run fails, inspect its `failed-jobs.log` and launch/cleanup artifacts; keep
the independent schedule until termination has been confirmed.
