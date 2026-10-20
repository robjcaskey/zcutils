# Diskpipe documentation HTTPS operations

`https://diskpipe.com/` and `https://www.diskpipe.com/` use an AWS-issued public
certificate through CloudFront. Both names serve the same documentation.
HTTP requests redirect to HTTPS. CloudFront fetches the generated static files
from GitHub Pages over HTTPS; publication remains GitHub Actions plus a
repository-scoped deploy key, without AWS credentials in CI.

## Architecture and ownership

The [Terraform module](main.tf) manages one non-exportable ACM public certificate
in `us-east-1`, its DNS validation records, one shared-IP CloudFront distribution,
an explicit Amazon CAA authorization, and the existing apex A/AAAA and www CNAME
records. The existing Route53 hosted
zone and domain registration remain outside this module. Domain registration,
registrant contacts, privacy and annual renewal are not modified by Terraform.

The origin is `https://robjcaskey.github.io/diskpipe-docs/`. CloudFront appends
incoming paths to `/diskpipe-docs`, sends the origin host rather than the viewer
host, and verifies GitHub's TLS certificate. The www CNAME points to `diskpipe.com`, whose A/AAAA aliases select CloudFront.
The public browser connection uses
the ACM certificate with `diskpipe.com` and `www.diskpipe.com` as its names.
The distribution accepts read methods only, compresses responses, supports IPv6
and requires modern TLS. Real missing pages remain 404s.

GitHub Pages must have **no custom domain**. Its settings use `cname: null`, and
the `gh-pages` repository must not contain a root `CNAME` file, even an empty one.
Restoring that file can redirect the origin back through CloudFront. The
publisher removes stale `CNAME` files instead of recreating them. The previous
GitHub certificate-polling workflow remains disabled; ACM manages renewal using
the retained DNS validation records.

## Expected cost

Pricing checked against the linked AWS pages on October 19, 2026:

- The [ACM public certificate](https://aws.amazon.com/certificate-manager/pricing/)
  has no certificate charge when used by CloudFront. Export is explicitly
  disabled. This module creates no AWS Private CA.
- [CloudFront pay-as-you-go](https://aws.amazon.com/cloudfront/pricing/) has no
  fixed distribution charge. Its monthly account-wide free allowance includes
  1 TB of data transfer out and 10 million HTTP/HTTPS requests. The allowance is
  shared with this account's other distributions; traffic above it is metered.
  Price Class 100 limits the selected edge regions. This is not a flat-rate
  subscription or a guaranteed zero total bill.
- The existing [Route53 public hosted zone](https://aws.amazon.com/route53/pricing/)
  normally costs $0.50 per month for the first 25 zones. Apex A/AAAA aliases to
  CloudFront have no DNS query charge; www CNAME and validation/other DNS queries
  use standard DNS query pricing. No extra hosted zone is created.
- The existing domain registration was $16 for its first year. Annual renewal
  remains a separate registrar charge and follows the registrar's current price.

There are no EC2 instances, load balancers, NAT gateways, S3 buckets, private CAs,
paid WAF resources, dedicated-IP TLS, functions or log ingestion resources in
this module. Cache expiry avoids routine invalidation requests. For a low-traffic
docs site within the shared allowance, incremental certificate and delivery cost
is expected to be $0; existing DNS and annual registration costs remain.

## Change infrastructure

Use the existing protected state rather than initializing a second deployment.
On the current operator workstation it is stored at
`/home/rob/.local/state/diskpipe-docs/terraform.tfstate` outside the repository.
The directory is accessible only by the operator. Retain a protected backup
before migrating workstations or removing this checkout. Do not commit state,
plans, credentials or `.terraform/` to Git.

Run from this module directory, with an authenticated AWS profile:

```bash
export AWS_PROFILE=slopmud-breakglass
terraform init -backend-config="path=/home/rob/.local/state/diskpipe-docs/terraform.tfstate"
terraform fmt -check
terraform validate
terraform plan -out=/tmp/diskpipe-https.tfplan
terraform apply /tmp/diskpipe-https.tfplan
terraform output
```

Inspect the saved plan before applying it. The initial migration imported the
existing records, then updated them in place; it did not create a second zone:

```bash
terraform import 'aws_route53_record.apex["A"]' Z02482853NUNWHLIEN98Q_diskpipe.com_A
terraform import 'aws_route53_record.apex["AAAA"]' Z02482853NUNWHLIEN98Q_diskpipe.com_AAAA
terraform import aws_route53_record.www Z02482853NUNWHLIEN98Q_www.diskpipe.com_CNAME
```

These import commands document the migration and are not needed with the
existing state. If state must be recovered, inventory and import the existing
ACM certificate, validation DNS records and CloudFront distribution as well;
do not apply from an empty state against existing infrastructure.

The initial ACM request failed because the old www CNAME pointed to
`robjcaskey.github.io`, whose inherited CAA policy allows other certificate
providers but excludes Amazon. The module points www at the apex, adds
`0 issue "amazon.com"` at the apex, and orders certificate creation after these
changes. [AWS documents CAA requirements](https://docs.aws.amazon.com/acm/latest/userguide/troubleshooting-caa.html).
This keeps Amazon authorized for future renewals as well as initial issuance.
A cached old CNAME may need its prior TTL to expire before a new request works.

Terraform waits for certificate issuance, CloudFront deployment and DNS changes.
Keep the ACM validation CNAMEs in DNS to allow automatic renewal. A routine
follow-up `terraform plan -detailed-exitcode` should exit 0 with no changes.

## Publish and verify

Commit Markdown and push to the source repository's `main` branch. The docs
workflow validates links, renders all tracked Diskpipe Markdown plus linked
shared docs, and pushes the generated files to `robjcaskey/diskpipe-docs` on
`gh-pages`. Pages stays enabled on that branch with no custom domain. No
Terraform apply is needed for documentation changes.

CloudFront's cache is capped at five minutes; GitHub Pages build/deployment and
its own caches can add their own publication delay. After GitHub Pages reports
the new build, verify the origin and both public names:

```bash
curl --fail --show-error --silent https://robjcaskey.github.io/diskpipe-docs/ -o /tmp/diskpipe-origin.html
bash infra/diskpipe-docs/verify-https.sh
```

Run the verification script from the repository root. It checks normal public
DNS resolution and certificate trust, both hostnames, HTTP redirects, CSS and
a nested documentation page. It does not disable certificate verification.
For certificate inspection:

```bash
openssl s_client -connect diskpipe.com:443 -servername diskpipe.com </dev/null 2>/dev/null | openssl x509 -noout -issuer -dates -ext subjectAltName
```

If a page is stale, compare the origin to the public site and allow the caches
to expire. If the origin redirects to diskpipe.com, remove the Pages custom
domain and root `CNAME`, then republish. If the public certificate fails, inspect
ACM status in `us-east-1`, its DNS validation records and the deployed CloudFront
certificate rather than enabling GitHub's separate certificate workflow.

## Retire the site

Move public DNS and validate the replacement HTTPS site before removing this
module. A deliberate `terraform destroy` removes this module's CloudFront
configuration, certificate and managed DNS records, including the imported
apex/www records. It does not unregister the domain or delete its hosted zone;
those keep their existing renewal and hosted-zone charges until managed
separately. CloudFront disable/delete propagation can take several minutes.
