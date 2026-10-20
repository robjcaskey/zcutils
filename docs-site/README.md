# Documentation sites

Run the same validation and rendering used by CI:

```bash
bash scripts/check-docs-site.sh
```

The Rust tool in [tools/zc-docs](../tools/zc-docs/Cargo.toml) has a committed
Cargo lockfile and a pinned CommonMark parser. The build uses two compilation
jobs and temporary build storage. It does not build the storage services.
An optional argument chooses the output directory. The outputs are the zcutils
site at `docs-site/dist/` and the Diskpipe site at `docs-site/dist/diskpipe/`.

## Diskpipe documentation discovery

Every Git-tracked Markdown file beneath `zccusan/` is rendered automatically.
No page allowlist needs updating when a guide is added. The builder follows
Markdown links recursively into shared repository documentation and copies
linked repository assets. Untracked laboratories, credentials and local test
outputs are excluded. Commit a new page before expecting CI to publish it;
local builds also include staged new files.

Links to Markdown become relative HTML links, including links with heading
fragments and links written as GitHub URLs to this repository's `main` branch.
An introductory page links to the complete generated documentation library.
Referenced source directories can still link to the repository, and external
projects retain their external URLs. Documentation links stay on Diskpipe.

Nested lists, block quotes, reference links, images, tables and fenced code
are supported through CommonMark. Heading anchors come from visible heading
text and receive deterministic suffixes when repeated. Raw HTML is escaped;
HTML comments are omitted. Mermaid fences remain readable source code.
Unfilled template links are displayed as text until their values and generated
artifacts exist. Missing ordinary local targets or heading fragments fail the
build before new output is emitted.

The landing pages live in `zccusan/docs/site/`. Stable homepage, Kubernetes,
standalone and day-two URLs are retained. Other storage pages keep their source
hierarchy, such as `docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.html`.
Shared documents live beneath `reference/`. The builder writes a
`site-manifest.txt` containing exactly the public files generated for deployment.

## Publication

GitHub Actions validates pull requests. Pushes to `main` and manual runs on
`main` publish both sites. Markdown changes anywhere, storage documentation and
its assets, shared docs, build-tool changes and source examples trigger this
workflow. The workflow is `.github/workflows/docs-site.yml`.

Diskpipe publishes at [diskpipe.com](https://diskpipe.com/) using the dedicated
`robjcaskey/diskpipe-docs` Pages repository and its `gh-pages` branch.
`scripts/publish-diskpipe-docs.sh` copies the generated manifest, publishes
nested pages and removes previously generated pages that have been deleted.
It removes the legacy Pages `CNAME` and preserves repository configuration.
CloudFront serves the public apex and www names with an ACM certificate, while
Pages retains its default `robjcaskey.github.io/diskpipe-docs/` origin URL.
A custom domain on Pages would redirect origin requests back to CloudFront;
do not restore `CNAME` or set a Pages custom domain. A repository-scoped deploy key is stored in the source repository's
`DISKPIPE_DOCS_DEPLOY_KEY` Actions secret.

The independent [zcutils site](https://robjcaskey.github.io/zcutils/) retains its
small curated landing-page set. Its publisher preserves Helm chart archives,
`index.yaml` and signatures on the existing `gh-pages` branch. The old day-two
URL continues forwarding to the storage documentation. The
[Zero Copy Cinematic Universe](universe.md) connects the two projects.

See [HTTPS infrastructure operations](../infra/diskpipe-docs/README.md) for the
Terraform module, certificate renewal, DNS ownership, costs and public checks.

## Local subset checks

The small dependency-free validator remains available for the curated pages:

```bash
rustc --edition=2024 scripts/zc-md.rs -o /tmp/zc-md
/tmp/zc-md --check docs/saml-realtime-revocation-debug.md
/tmp/zc-md --render docs/saml-realtime-revocation-debug.md /tmp/saml.html
```

That strict subset supports ATX headings, wrapped paragraphs, single-level
lists, pipe tables, inline code, asterisk emphasis, inline links and backtick
fences. It requires one title and consistent heading levels. Full Diskpipe
site generation uses the Cargo build to support existing CommonMark guides.

## Shared appearance

Both projects use the existing layout and `docs-site/site.css`: cool white,
slate-gray surfaces and teal links. System light/dark preference is followed
automatically, including changes while the page is open. Code, tables,
navigation and focus states have a dark variant. Print keeps the light palette.
There is no theme script or saved override.

## Diskpipe landing page

`docs-site/diskpipe-landing.html` is the checked-in interactive Diskpipe front
page. The full renderer publishes it as `index.html` and renders the Markdown
homepage and documentation library as `home.html`. The publication manifest
includes both, so the normal documentation CI deploys the onboarding flow along
with every generated guide. Edit the checked-in HTML rather than a local preview
or the deployment repository.
