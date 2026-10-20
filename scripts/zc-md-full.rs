//! Full documentation discovery and CommonMark rendering for Diskpipe.
use super::{Result, escape, normalized, slug};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const MAIN: [(&str, &str, &str); 5] = [
    ("zccusan/docs/site/index.md", "index.html", "Diskpipe"),
    (
        "zccusan/docs/site/kubernetes.md",
        "kubernetes.html",
        "With Kubernetes",
    ),
    (
        "zccusan/docs/site/standalone.md",
        "standalone.html",
        "Without Kubernetes",
    ),
    (
        "zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md",
        "day-2.html",
        "Day-two operations",
    ),
    (
        "docs-site/universe.md",
        "universe.html",
        "Zero Copy Cinematic Universe",
    ),
];

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}
fn markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}
fn route(source: &Path) -> String {
    if let Some((_, output, _)) = MAIN.iter().find(|(s, _, _)| Path::new(s) == source) {
        return output.to_string();
    }
    let path = if let Ok(local) = source.strip_prefix("zccusan") {
        local.to_path_buf()
    } else {
        Path::new("reference").join(source)
    };
    path.with_extension("html").to_string_lossy().into_owned()
}
fn prefix(output: &str) -> String {
    "../".repeat(Path::new(output).components().count() - 1)
}
fn decoded(s: &str) -> Result<String> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut at = 0;
    while at < b.len() {
        if b[at] == b'%' {
            if at + 2 >= b.len() {
                return Err(format!("bad URL escape: {s}"));
            }
            let hex = std::str::from_utf8(&b[at + 1..at + 3]).map_err(|_| "bad URL escape")?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| "bad URL escape")?);
            at += 3;
        } else {
            out.push(b[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "URL path is not UTF-8".into())
}
fn encoded(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

struct Target {
    path: PathBuf,
    fragment: Option<String>,
    query: Option<String>,
}
// A None result is an external URL, preserved after scheme validation.
fn target(source: &Path, href: &str) -> Result<Option<Target>> {
    if href.chars().any(|c| c.is_control() || "<>\"\\".contains(c)) || href.starts_with("//") {
        return Err(format!("unsafe link: {href}"));
    }
    let internal = [
        "https://github.com/robjcaskey/zcutils/blob/main/",
        "https://github.com/robjcaskey/zcutils/tree/main/",
        "https://raw.githubusercontent.com/robjcaskey/zcutils/main/",
    ]
    .iter()
    .find_map(|base| href.strip_prefix(base));
    if internal.is_none()
        && (href.starts_with("https://")
            || href.starts_with("http://")
            || href.starts_with("mailto:"))
    {
        return Ok(None);
    }
    let href = internal.unwrap_or(href);
    let (path_query, fragment) = href
        .split_once('#')
        .map_or((href, None), |(p, f)| (p, Some(f.to_string())));
    let (path, query) = path_query
        .split_once('?')
        .map_or((path_query, None), |(p, q)| (p, Some(q.to_string())));
    let path = decoded(path)?;
    if path.contains(':') || path.starts_with('~') {
        return Err(format!("unsafe link scheme: {href}"));
    }
    let resolved = if internal.is_some() || path.starts_with('/') {
        normalized(Path::new(""), path.trim_start_matches('/'))?
    } else if path.is_empty() {
        source.to_path_buf()
    } else {
        normalized(source.parent().unwrap_or(Path::new("")), &path)?
    };
    Ok(Some(Target {
        path: resolved,
        fragment,
        query,
    }))
}

struct Heading {
    id: String,
    label: String,
    level: HeadingLevel,
}
fn headings(events: &[Event<'_>]) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut used = BTreeSet::new();
    for (i, event) in events.iter().enumerate() {
        if let Event::Start(Tag::Heading { level, .. }) = event {
            let mut text = String::new();
            for e in &events[i + 1..] {
                match e {
                    Event::End(TagEnd::Heading(_)) => break,
                    Event::Text(s) | Event::Code(s) => text.push_str(s),
                    Event::SoftBreak | Event::HardBreak => text.push(' '),
                    _ => (),
                }
            }
            let base = slug(&text);
            let base = if base.is_empty() {
                "section".to_string()
            } else {
                base
            };
            let mut id = base.clone();
            let mut n = 0;
            while !used.insert(id.clone()) {
                n += 1;
                id = format!("{base}-{n}");
            }
            out.push(Heading {
                id,
                label: text,
                level: *level,
            });
        }
    }
    out
}
struct Doc {
    text: String,
    output: String,
    title: String,
    headings: Vec<Heading>,
}
impl Doc {
    fn new(source: &Path, text: String) -> Self {
        let events: Vec<_> = Parser::new_ext(&text, options()).collect();
        let headings = headings(&events);
        let title = headings
            .iter()
            .find(|h| h.level == HeadingLevel::H1)
            .or_else(|| headings.first())
            .map(|h| h.label.clone())
            .unwrap_or_else(|| source.file_stem().unwrap().to_string_lossy().into_owned());
        Self {
            text,
            output: route(source),
            title,
            headings,
        }
    }
}
fn placeholder(href: &str) -> bool {
    href.contains("{{") && href.contains("}}")
}
fn links(text: &str) -> Vec<String> {
    Parser::new_ext(text, options())
        .filter_map(|e| match e {
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                (!placeholder(&dest_url)).then(|| dest_url.into_string())
            }
            _ => None,
        })
        .collect()
}
fn local_file(root: &Path, tracked: &BTreeSet<PathBuf>, target: &mut Target) -> Result<()> {
    if root.join(&target.path).is_dir() {
        if root.join(&target.path).join("README.md").is_file() {
            target.path.push("README.md");
        } else {
            return Ok(());
        } // An explicit source-directory reference remains a repository link.
    }
    if !tracked.contains(&target.path) {
        return Err(format!(
            "missing or untracked local target: {}",
            target.path.display()
        ));
    }
    let abs = root.join(&target.path);
    if fs::symlink_metadata(&abs)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err(format!(
            "symlink documentation target: {}",
            target.path.display()
        ));
    }
    if !abs
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(root)
    {
        return Err("documentation link escapes repository".into());
    }
    Ok(())
}
fn strip_comments(raw: &str, inside: &mut bool) -> String {
    let mut out = String::new();
    let mut rest = raw;
    loop {
        if *inside {
            let Some(end) = rest.find("-->") else {
                break;
            };
            rest = &rest[end + 3..];
            *inside = false;
        } else if let Some(start) = rest.find("<!--") {
            out.push_str(&rest[..start]);
            rest = &rest[start + 4..];
            *inside = true;
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}
fn render_doc(doc: &Doc, link: &dyn Fn(&str) -> Result<String>) -> Result<String> {
    let mut events = Vec::new();
    let mut heading = 0;
    let mut placeholder_links = Vec::new();
    let mut inside_comment = false;
    for e in Parser::new_ext(&doc.text, options()) {
        let e = match e {
            Event::Start(Tag::Heading {
                level,
                classes,
                attrs,
                ..
            }) => {
                let id = doc.headings[heading].id.clone();
                heading += 1;
                Event::Start(Tag::Heading {
                    level,
                    id: Some(id.into()),
                    classes,
                    attrs,
                })
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let is_placeholder = placeholder(&dest_url)
                    || doc.output.ends_with(".template.html") && link(&dest_url).is_err();
                placeholder_links.push(is_placeholder);
                if is_placeholder {
                    Event::Html("<span title=\"Template placeholder\">".into())
                } else {
                    Event::Start(Tag::Link {
                        link_type,
                        dest_url: link(&dest_url)?.into(),
                        title,
                        id,
                    })
                }
            }
            Event::End(TagEnd::Link) if placeholder_links.pop().unwrap_or(false) => {
                Event::Html("</span>".into())
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Image {
                link_type,
                dest_url: link(&dest_url)?.into(),
                title,
                id,
            }),
            Event::Html(raw) | Event::InlineHtml(raw) => {
                let raw = strip_comments(&raw, &mut inside_comment);
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed, "<br>" | "<br/>" | "<br />") {
                    Event::HardBreak
                } else {
                    Event::Text(raw.into())
                }
            }
            Event::Start(Tag::Table(align)) => {
                events.push(Event::Html("<div class=\"table-scroll\">".into()));
                Event::Start(Tag::Table(align))
            }
            Event::End(TagEnd::Table) => {
                events.push(Event::End(TagEnd::Table));
                Event::Html("</div>".into())
            }
            e => e,
        };
        events.push(e);
    }
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    Ok(body)
}
fn shell(doc: &Doc, body: &str, catalog: &str) -> String {
    let prefix = prefix(&doc.output);
    let nav = MAIN
        .iter()
        .map(|(_, out, label)| {
            format!(
                "<a href=\"{prefix}{out}\"{}>{}</a>",
                if *out == doc.output {
                    " aria-current=\"page\""
                } else {
                    ""
                },
                escape(label)
            )
        })
        .collect::<String>();
    let toc = doc
        .headings
        .iter()
        .filter(|h| h.level == HeadingLevel::H2)
        .map(|h| format!("<a href=\"#{}\">{}</a>", escape(&h.id), escape(&h.label)))
        .collect::<String>();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"description\" content=\"Diskpipe documentation and operating guides.\"><title>{} · Diskpipe</title><link rel=\"stylesheet\" href=\"{prefix}site.css\"></head><body><a class=\"skip\" href=\"#content\">Skip to content</a><header><a class=\"brand\" href=\"{prefix}index.html\">Diskpipe<span>STORAGE / OPERATIONS</span></a><nav class=\"header-links\" aria-label=\"Project links\"><a class=\"project-link\" href=\"https://robjcaskey.github.io/zcutils/\">zcutils docs ↗</a><a class=\"repo\" href=\"https://github.com/robjcaskey/zcutils\">Repository ↗</a></nav></header><div class=\"layout\"><aside><p class=\"eyebrow\">The guides</p><nav aria-label=\"Guides\">{nav}<a href=\"{prefix}index.html#documentation-library\">All documentation</a></nav></aside><main id=\"content\">{body}{catalog}<footer>Built from Markdown with Rust. No client-side dependencies.</footer></main><nav class=\"toc\" aria-label=\"On this page\"><p class=\"eyebrow\">On this page</p>{toc}</nav></div></body></html>\n",
        escape(&doc.title)
    )
}

pub fn site(root: &Path, output: &Path) -> Result<()> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let git = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|e| e.to_string())?;
    if !git.status.success() {
        return Err("cannot discover tracked documentation".into());
    }
    let tracked: BTreeSet<_> = std::str::from_utf8(&git.stdout)
        .map_err(|_| "non-UTF8 tracked path")?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect();
    let landing_path = PathBuf::from("docs-site/diskpipe-landing.html");
    let landing = if tracked.contains(&landing_path) {
        let mut t = Target { path: landing_path.clone(), fragment: None, query: None };
        local_file(&root, &tracked, &mut t)?;
        Some(fs::read_to_string(root.join(&landing_path)).map_err(|e| e.to_string())?)
    } else { None };
    let mut pending: BTreeSet<_> = tracked
        .iter()
        .filter(|p| p.starts_with("zccusan") && markdown(p))
        .cloned()
        .collect();
    pending.extend(MAIN.iter().map(|(p, _, _)| PathBuf::from(p)));
    let mut docs = BTreeMap::new();
    let mut assets = BTreeSet::new();
    while let Some(source) = pending.pop_first() {
        if docs.contains_key(&source) {
            continue;
        }
        let mut self_target = Target {
            path: source.clone(),
            fragment: None,
            query: None,
        };
        local_file(&root, &tracked, &mut self_target)?;
        let text = fs::read_to_string(root.join(&source))
            .map_err(|e| format!("{}: {e}", source.display()))?;
        for href in links(&text) {
            if let Some(mut t) =
                target(&source, &href).map_err(|e| format!("{}: {e}", source.display()))?
            {
                if let Err(e) = local_file(&root, &tracked, &mut t) {
                    if source.to_string_lossy().ends_with(".template.md") {
                        continue;
                    }
                    return Err(format!("{} -> {href}: {e}", source.display()));
                }
                if markdown(&t.path) {
                    if !docs.contains_key(&t.path) {
                        pending.insert(t.path);
                    }
                } else if root.join(&t.path).is_file() {
                    assets.insert(t.path);
                }
            }
        }
        let mut doc = Doc::new(&source, text);
        if landing.is_some() && doc.output == "index.html" {
            doc.output = "home.html".into();
        }
        docs.insert(source.clone(), doc);
    }
    let mut routes = BTreeSet::new();
    for doc in docs.values() {
        if !routes.insert(doc.output.clone()) {
            return Err(format!("duplicate HTML route: {}", doc.output));
        }
    }
    let catalog = format!(
        "<h2 id=\"documentation-library\">Documentation library</h2><ul>{}</ul>",
        docs.values()
            .map(|d| format!(
                "<li><a href=\"{}\">{}</a></li>",
                encoded(&d.output),
                escape(&d.title)
            ))
            .collect::<String>()
    );
    let mut rendered = Vec::new();
    for (source, doc) in &docs {
        let link = |href: &str| -> Result<String> {
            let Some(mut t) = target(source, href)? else {
                return Ok(href.to_string());
            };
            local_file(&root, &tracked, &mut t)?;
            let dest = if let Some(other) = docs.get(&t.path) {
                if let Some(fragment) = &t.fragment {
                    let fragment = decoded(fragment)?;
                    if !fragment.is_empty() && !other.headings.iter().any(|h| h.id == fragment) {
                        return Err(format!("missing heading: {href}"));
                    }
                }
                other.output.clone()
            } else if root.join(&t.path).is_dir() {
                return Ok(format!(
                    "https://github.com/robjcaskey/zcutils/tree/main/{}",
                    encoded(&t.path.to_string_lossy())
                ));
            } else {
                format!("assets/repository/{}", t.path.display())
            };
            Ok(format!(
                "{}{}{}{}",
                prefix(&doc.output),
                encoded(&dest),
                t.query.map_or(String::new(), |q| format!("?{q}")),
                t.fragment.map_or(String::new(), |f| format!("#{f}"))
            ))
        };
        let body = render_doc(doc, &link).map_err(|e| format!("{}: {e}", source.display()))?;
        rendered.push((
            doc.output.clone(),
            shell(
                doc,
                &body,
                if doc.output == "index.html" || doc.output == "home.html" {
                    &catalog
                } else {
                    ""
                },
            ),
        ));
    }
    if let Some(html) = landing {
        for (_, contents) in &mut rendered {
            *contents = contents.replace("index.html#documentation-library", "home.html#documentation-library");
        }
        rendered.push(("index.html".into(), html));
    }
    // No output is written until all sources and references have validated.
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    let mut manifest = Vec::new();
    for (name, contents) in rendered {
        let dest = output.join(&name);
        fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(dest, contents).map_err(|e| e.to_string())?;
        manifest.push(name);
    }
    for asset in assets {
        let name = format!("assets/repository/{}", asset.display());
        let dest = output.join(&name);
        fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::copy(root.join(asset), dest).map_err(|e| e.to_string())?;
        manifest.push(name);
    }
    fs::copy(root.join("docs-site/site.css"), output.join("site.css"))
        .map_err(|e| e.to_string())?;
    manifest.push("site.css".into());
    manifest.sort();
    fs::write(output.join("site-manifest.txt"), manifest.join("\n") + "\n")
        .map_err(|e| e.to_string())?;
    println!(
        "DISKPIPE_SITE_PASS pages={} output={}",
        docs.len(),
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commonmark_and_html_are_safe() {
        let d=Doc::new(Path::new("sample.md"),"# Sample\n\n> **Warning**\n\n- parent\n  - child\n\n<!-- hidden\nsecond comment line\n-->\n\n<script>alert(1)</script>\n\n```html\n<img onerror=bad>\n```".into());
        let html = render_doc(&d, &|s| Ok(s.into())).unwrap();
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<ul>"));
        assert!(!html.contains("<!-- hidden"));
        assert!(!html.contains("second comment line"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
    #[test]
    fn github_links_and_duplicate_headings() {
        let t = target(
            Path::new("docs/a.md"),
            "https://github.com/robjcaskey/zcutils/blob/main/zccusan/README.md#hello",
        )
        .unwrap()
        .unwrap();
        assert_eq!(t.path, Path::new("zccusan/README.md"));
        let d = Doc::new(
            Path::new("a.md"),
            "# `Title`\n\n## Repeat\n\n## Repeat\n".into(),
        );
        assert_eq!(d.title, "Title");
        assert_eq!(d.headings[2].id, "repeat-1");
        assert!(target(Path::new("a.md"), "javascript:alert(1)").is_err());
        assert!(target(Path::new("a.md"), "../outside.md").is_err());
    }
    #[test]
    fn discovers_tracked_docs_and_follows_links_without_publishing_untracked_files() {
        let root = std::env::temp_dir().join(format!(
            "diskpipe-docs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        for (source, _, _) in MAIN {
            let path = repo.join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "# Guide\n").unwrap();
        }
        fs::write(
            repo.join("zccusan/docs/site/index.md"),
            "# Diskpipe\n\n[New](../new.md)\n",
        )
        .unwrap();
        fs::write(repo.join("zccusan/docs/new.md"), "# New\n\n> Note\n\n- Parent\n  - Child\n\n[Extra](../../docs/extra.md#an-extra)\n\n[Config](../examples/config.yaml)\n").unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(
            repo.join("docs/extra.md"),
            "# An extra\n\n[Return](../zccusan/docs/new.md#new)\n",
        )
        .unwrap();
        fs::create_dir_all(repo.join("zccusan/examples")).unwrap();
        fs::write(repo.join("zccusan/examples/config.yaml"), "example: true\n").unwrap();
        fs::write(repo.join("docs-site/site.css"), "body{}\n").unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        fs::write(repo.join("zccusan/docs/private.md"), "# Private\n").unwrap();
        let out = root.join("site");
        site(&repo, &out).unwrap();
        assert!(
            fs::read_to_string(out.join("docs/new.html"))
                .unwrap()
                .contains("../reference/docs/extra.html#an-extra")
        );
        assert!(
            out.join("assets/repository/zccusan/examples/config.yaml")
                .is_file()
        );
        assert!(!out.join("docs/private.html").exists());
        assert!(
            fs::read_to_string(out.join("site-manifest.txt"))
                .unwrap()
                .contains("docs/new.html")
        );
        fs::write(repo.join("docs/extra.md"), "# Changed anchor\n").unwrap();
        let failed = root.join("failed");
        assert!(
            site(&repo, &failed)
                .unwrap_err()
                .contains("missing heading")
        );
        assert!(!failed.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
