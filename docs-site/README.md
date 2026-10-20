# Documentation site

Build and validate the project documentation with the system Rust compiler:

```bash
bash scripts/check-docs-site.sh
```

The script runs the renderer's focused tests, validates the eight allowlisted
Markdown pages and their local links/anchors, and emits `docs-site/dist`.
It uses only `rustc` and the Rust standard library; no Cargo dependencies,
JavaScript packages, cloud resources or storage-service build are needed.
An optional argument selects another output directory. CI uses the same script.

The supported subset is ATX headings, paragraphs with wrapped lines, single-level
dash/numbered lists, pipe tables, horizontal rules, inline code, asterisk emphasis,
inline links and three-backtick code fences. Mermaid fences are shown as escaped
diagram source. One title, consistent heading levels and unique anchors are
required. Raw HTML, images, reference links, nested lists, block quotes and
indented code are unsupported; use the declared subset. HTML/code text is escaped
and link schemes are restricted. Local published-guide links become HTML links;
other existing repository targets link to GitHub.

For just a file check or an HTML fragment:

```bash
rustc --edition=2024 scripts/zc-md.rs -o /tmp/zc-md
/tmp/zc-md --check docs/saml-realtime-revocation-debug.md
/tmp/zc-md --render docs/saml-realtime-revocation-debug.md /tmp/saml.html
```

GitHub Actions publishes this site at https://robjcaskey.github.io/zcutils/.
Pull requests validate and build; pushes to `main` and manual runs on `main`
also publish. `scripts/publish-docs-pages.sh` updates only the explicitly allowlisted generated
HTML/CSS files and `.nojekyll` on the existing `gh-pages` branch, preserving
`index.yaml`, chart archives and signatures. It explicitly requests a Pages
build using the workflow's `pages: write` permission, because commits pushed
with `GITHUB_TOKEN` do not automatically start a branch-based Pages build.

Pages must remain configured to deploy `gh-pages` at `/`. This is the existing
repository setting and also serves the Helm chart repository. No S3 bucket,
Terraform, access-key secret or external hosting service is required.
Never publish private laboratory directories, `lab.json`, credentials or SQLite
databases. Only the generated allowlisted HTML and CSS are deployed.

The zcutils home is at `/zcutils/`; zccusan owns its documentation home at
`/zcutils/zccusan/`, with separate with-Kubernetes and without-Kubernetes pages.
Its landing-page sources live under `zccusan/docs/site/` so they can move with
that project. Shared project relationships live in `docs-site/universe.md`.
The old `/day-2.html` URL forwards to `/zccusan/day-2.html`, preserving fragments.
Each project has its own brand and navigation. Published links and stylesheet
paths are relative, so the site also works beneath the GitHub project prefix.
