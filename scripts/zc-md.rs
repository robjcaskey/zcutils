//! Small, dependency-free Markdown validator and static documentation renderer.
//! The supported dialect is documented in docs-site/README.md.
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
type Linker<'a> = dyn Fn(&str) -> Result<String> + 'a;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn safe_link(s: &str) -> Result<String> {
    if s.is_empty()
        || s.chars()
            .any(|c| c.is_control() || c.is_whitespace() || "<>\"'\\".contains(c))
        || s.contains('&') && !s.starts_with("https://") && !s.starts_with("http://")
        || s.starts_with('/')
        || s.starts_with('~')
        || s.contains(':') && !s.starts_with("https://") && !s.starts_with("http://")
    {
        return Err("link must be a relative path, fragment, or HTTP(S) URL".into());
    }
    Ok(s.into())
}

fn inline(s: &str, link: &Linker, depth: usize) -> Result<String> {
    if depth > 8 {
        return Err("inline nesting exceeds eight levels".into());
    }
    let mut out = String::new();
    let mut at = 0;
    while at < s.len() {
        let rest = &s[at..];
        if rest.starts_with('\\') {
            let c = rest[1..]
                .chars()
                .next()
                .ok_or("dangling backslash escape")?;
            if !c.is_ascii_punctuation() {
                return Err("escape must precede punctuation".into());
            }
            out.push_str(&escape(&c.to_string()));
            at += 1 + c.len_utf8();
        } else if rest.starts_with('`') {
            let end = rest[1..].find('`').ok_or("unclosed inline code")? + 1;
            if end == 1 {
                return Err("empty inline code / multiple-backtick spans unsupported".into());
            }
            out.push_str(&format!("<code>{}</code>", escape(&rest[1..end])));
            at += end + 1;
        } else if rest.starts_with("![") {
            return Err("images unsupported; use an ordinary link".into());
        } else if rest.starts_with('[') {
            let close = rest
                .find("](")
                .ok_or("link requires [label](destination)")?;
            let start = close + 2;
            let mut level = 1;
            let mut end = None;
            for (offset, c) in rest[start..].char_indices() {
                if c == '(' {
                    level += 1;
                }
                if c == ')' {
                    level -= 1;
                }
                if level == 0 {
                    end = Some(start + offset);
                    break;
                }
            }
            let end = end.ok_or("unclosed link destination")?;
            let href = safe_link(&rest[start..end])?;
            out.push_str(&format!(
                "<a href=\"{}\">{}</a>",
                escape(&link(&href)?),
                inline(&rest[1..close], link, depth + 1)?
            ));
            at += end + 1;
        } else if rest.starts_with('*') {
            let mark = if rest.starts_with("**") { "**" } else { "*" };
            let end = rest[mark.len()..].find(mark).ok_or("unclosed emphasis")? + mark.len();
            if end == mark.len() {
                return Err("empty emphasis".into());
            }
            let tag = if mark.len() == 2 { "strong" } else { "em" };
            out.push_str(&format!(
                "<{tag}>{}</{tag}>",
                inline(&rest[mark.len()..end], link, depth + 1)?
            ));
            at += end + mark.len();
        } else if rest.starts_with('<') {
            return Err(
                "raw HTML and autolinks unsupported; use fenced code or Markdown links".into(),
            );
        } else {
            let c = rest.chars().next().unwrap();
            out.push_str(&escape(&c.to_string()));
            at += c.len_utf8();
        }
    }
    Ok(out)
}

fn slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn item(s: &str) -> Option<(bool, &str)> {
    if let Some(text) = s.strip_prefix("- ") {
        return Some((false, text));
    }
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        return s[digits..].strip_prefix(". ").map(|text| (true, text));
    }
    None
}

fn cells(s: &str) -> Vec<&str> {
    s.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}
fn separator(s: &str) -> bool {
    s.starts_with('|')
        && cells(s).iter().all(|s| {
            let s = s.trim_matches(':');
            s.len() >= 3 && s.bytes().all(|b| b == b'-')
        })
}
fn block(s: &str) -> bool {
    s.is_empty()
        || s.starts_with('#')
        || s.starts_with("```")
        || s.starts_with('|')
        || s.starts_with('>')
        || s.starts_with("~~~")
        || s.starts_with('<')
        || s.starts_with("![")
        || s == "---"
        || item(s).is_some()
}

struct Document {
    title: String,
    html: String,
    headings: Vec<(String, String)>,
}

fn render(source: &str, link: &Linker) -> Result<Document> {
    let lines: Vec<_> = source.lines().collect();
    let mut at = 0;
    let mut html = String::new();
    let mut title = String::new();
    let mut headings = Vec::new();
    let mut ids = BTreeSet::new();
    let mut previous_level = 0;
    while at < lines.len() {
        let line = lines[at].trim_end();
        let number = at + 1;
        let result: Result<()> = (|| {
            if line.is_empty() {
                at += 1;
                return Ok(());
            }
            if title.is_empty() && !line.starts_with("# ") {
                return Err("first block must be a level-one title".into());
            }
            if line.starts_with("```") {
                let lang = &line[3..];
                if !lang
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err("fence must be three backticks and an optional language".into());
                }
                at += 1;
                let start = at;
                while at < lines.len() && lines[at] != "```" {
                    at += 1;
                }
                if at == lines.len() {
                    return Err("unclosed code fence".into());
                }
                html.push_str(&format!(
                    "<pre><code class=\"language-{}\">{}</code></pre>\n",
                    escape(lang),
                    escape(&lines[start..at].join("\n"))
                ));
                at += 1;
            } else if line.starts_with('#') {
                let level = line.bytes().take_while(|b| *b == b'#').count();
                if level > 6 || !line[level..].starts_with(' ') || line[level..].trim().is_empty() {
                    return Err("heading requires one to six hashes, a space and text".into());
                }
                if level > previous_level + 1 {
                    return Err("heading level skips its parent".into());
                }
                let text = line[level + 1..].trim();
                if level == 1 {
                    if !title.is_empty() {
                        return Err("only one level-one title is allowed".into());
                    }
                    title = text.into();
                }
                let id = slug(text);
                if id.is_empty() || !ids.insert(id.clone()) {
                    return Err("empty or duplicate heading anchor".into());
                }
                html.push_str(&format!(
                    "<h{level} id=\"{}\">{}</h{level}>\n",
                    escape(&id),
                    inline(text, link, 0)?
                ));
                if level == 2 {
                    headings.push((id, text.into()));
                }
                previous_level = level;
                at += 1;
            } else if line == "---" {
                html.push_str("<hr>\n");
                at += 1;
            } else if line.starts_with('|') {
                let header = cells(line);
                if at + 1 == lines.len()
                    || !separator(lines[at + 1])
                    || cells(lines[at + 1]).len() != header.len()
                {
                    return Err("table needs a matching separator row".into());
                }
                html.push_str("<div class=\"table-scroll\"><table><thead><tr>");
                for cell in &header {
                    html.push_str(&format!("<th>{}</th>", inline(cell, link, 0)?));
                }
                html.push_str("</tr></thead><tbody>\n");
                at += 2;
                while at < lines.len() && lines[at].starts_with('|') {
                    let row = cells(lines[at]);
                    if row.len() != header.len() {
                        return Err(format!(
                            "table row {} has the wrong number of cells",
                            at + 1
                        ));
                    }
                    html.push_str("<tr>");
                    for cell in row {
                        html.push_str(&format!("<td>{}</td>", inline(cell, link, 0)?));
                    }
                    html.push_str("</tr>\n");
                    at += 1;
                }
                html.push_str("</tbody></table></div>\n");
            } else if let Some((ordered, _)) = item(line) {
                let tag = if ordered { "ol" } else { "ul" };
                let start = if ordered {
                    line.split_once('.')
                        .unwrap()
                        .0
                        .parse::<u32>()
                        .map_err(|_| "list start is too large")?
                } else {
                    1
                };
                if ordered && start != 1 {
                    html.push_str(&format!("<ol start=\"{start}\">\n"));
                } else {
                    html.push_str(&format!("<{tag}>\n"));
                }
                while at < lines.len() {
                    let Some((kind, text)) = item(lines[at]) else {
                        break;
                    };
                    if kind != ordered {
                        break;
                    }
                    let mut text = text.to_owned();
                    at += 1;
                    while at < lines.len() && !block(lines[at].trim_end()) {
                        if item(lines[at].trim_start()).is_some() {
                            return Err("nested lists unsupported".into());
                        }
                        text.push(' ');
                        text.push_str(lines[at].trim());
                        at += 1;
                    }
                    html.push_str(&format!("<li>{}</li>\n", inline(&text, link, 0)?));
                    while at < lines.len() && lines[at].is_empty() {
                        at += 1;
                    }
                }
                html.push_str(&format!("</{tag}>\n"));
            } else {
                if line.starts_with('>')
                    || line.starts_with("~~~")
                    || line.starts_with("  ")
                    || line.starts_with('\t')
                    || line.starts_with("+ ")
                    || line.starts_with("* ")
                {
                    return Err(
                        "block quotes, alternate fences and indented blocks unsupported".into(),
                    );
                }
                let mut text = line.to_owned();
                at += 1;
                while at < lines.len() && !block(lines[at].trim_end()) {
                    text.push(' ');
                    text.push_str(lines[at].trim());
                    at += 1;
                }
                html.push_str(&format!("<p>{}</p>\n", inline(&text, link, 0)?));
            }
            Ok(())
        })();
        result.map_err(|e| format!("line {number}: {e}"))?;
    }
    if title.is_empty() {
        return Err("line 1: document requires a title".into());
    }
    Ok(Document {
        title,
        html,
        headings,
    })
}

const PAGES: [(&str, &str, &str); 8] = [
    ("docs-site/index.md", "index.html", "zcutils"),
    (
        "docs-site/universe.md",
        "universe.html",
        "Zero Copy Cinematic Universe",
    ),
    (
        "zccusan/docs/site/index.md",
        "zccusan/index.html",
        "Diskpipe",
    ),
    (
        "zccusan/docs/site/kubernetes.md",
        "zccusan/kubernetes.html",
        "With Kubernetes",
    ),
    (
        "zccusan/docs/site/standalone.md",
        "zccusan/standalone.html",
        "Without Kubernetes",
    ),
    (
        "docs/saml-realtime-revocation-debug.md",
        "saml.html",
        "SAML & revocation",
    ),
    (
        "zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md",
        "zccusan/day-2.html",
        "Day-two operations",
    ),
    (
        "docs/federated-identity-breakglass-debug.md",
        "federation.html",
        "Federation laboratory",
    ),
];

fn normalized(base: &Path, target: &str) -> Result<PathBuf> {
    let mut out = base.to_owned();
    for part in Path::new(target).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => (),
            Component::ParentDir => {
                if !out.pop() {
                    return Err("link escapes repository".into());
                }
            }
            _ => return Err("absolute local links unsupported".into()),
        }
    }
    Ok(out)
}

fn site(root: &Path, output: &Path, diskpipe: bool) -> Result<()> {
    let pages: Vec<_> = PAGES
        .iter()
        .filter_map(|&(source, file, label)| {
            if diskpipe {
                file.strip_prefix("zccusan/")
                    .map(|file| (source, file, label))
                    .or_else(|| (file == "universe.html").then_some((source, file, label)))
            } else {
                Some((source, file, label))
            }
        })
        .collect();
    let sources: Vec<_> = pages
        .iter()
        .map(|(path, _, _)| fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}")))
        .collect::<Result<_>>()?;
    // First pass validates the dialect and builds the complete anchor set.
    let docs: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(i, source)| render(source, &safe_link).map_err(|e| format!("{}: {e}", pages[i].0)))
        .collect::<Result<_>>()?;
    let mut rendered = Vec::new();
    for (i, (path, filename, _)) in pages.iter().enumerate() {
        let is_zccusan = diskpipe || filename.starts_with("zccusan/");
        let prefix = if is_zccusan && !diskpipe { "../" } else { "" };
        let brand = if is_zccusan { "Diskpipe" } else { "zcutils" };
        let home = if diskpipe {
            "index.html"
        } else if is_zccusan {
            "zccusan/index.html"
        } else {
            "index.html"
        };
        let subtitle = if is_zccusan {
            "STORAGE / OPERATIONS"
        } else {
            "ZERO-COPY TOOLS / GUIDES"
        };
        let project_link = if is_zccusan {
            "<a class=\"project-link\" href=\"https://robjcaskey.github.io/zcutils/\">zcutils docs ↗</a>"
        } else {
            "<a class=\"project-link\" href=\"https://diskpipe.com/\">Diskpipe docs ↗</a>"
        };
        let linker = |href: &str| -> Result<String> {
            if href.starts_with("http://") || href.starts_with("https://") {
                return Ok(href.into());
            }
            let (target, fragment) = href
                .split_once('#')
                .map_or((href, None), |(p, f)| (p, Some(f)));
            let resolved = if target.is_empty() {
                PathBuf::from(path)
            } else {
                normalized(Path::new(path).parent().unwrap(), target)?
            };
            let absolute = root
                .join(&resolved)
                .canonicalize()
                .map_err(|_| format!("missing local link target: {href}"))?;
            if !absolute.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
                return Err("link resolves outside repository".into());
            }
            if let Some(j) = pages.iter().position(|p| Path::new(p.0) == resolved) {
                if let Some(id) = fragment {
                    if !docs[j].html.contains(&format!("id=\"{}\"", escape(id))) {
                        return Err(format!("missing heading: {href}"));
                    }
                }
                return Ok(format!(
                    "{}{}{}",
                    prefix,
                    pages[j].1,
                    fragment.map_or(String::new(), |f| format!("#{f}"))
                ));
            }
            if let Some((_, file, _)) = PAGES.iter().find(|p| Path::new(p.0) == resolved) {
                return Ok(format!(
                    "https://robjcaskey.github.io/zcutils/{file}{}",
                    fragment.map_or(String::new(), |f| format!("#{f}"))
                ));
            }
            Ok(format!(
                "https://github.com/robjcaskey/zcutils/blob/main/{}{}",
                resolved.display(),
                fragment.map_or(String::new(), |f| format!("#{f}"))
            ))
        };
        let doc = render(&sources[i], &linker).map_err(|e| format!("{path}: {e}"))?;
        let nav = pages
            .iter()
            .filter(|(_, file, _)| {
                diskpipe || file.starts_with("zccusan/") == is_zccusan || *file == "universe.html"
            })
            .map(|(_, file, label)| {
                format!(
                    "<a href=\"{prefix}{file}\"{}>{}</a>",
                    if file == filename {
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
            .map(|(id, label)| format!("<a href=\"#{}\">{}</a>", escape(id), escape(label)))
            .collect::<String>();
        rendered.push((filename, format!("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"description\" content=\"{brand} documentation and operating guides.\"><title>{} · {brand}</title><link rel=\"stylesheet\" href=\"{prefix}site.css\"></head><body><a class=\"skip\" href=\"#content\">Skip to content</a><header><a class=\"brand\" href=\"{prefix}{home}\">{brand}<span>{subtitle}</span></a><nav class=\"header-links\" aria-label=\"Project links\">{project_link}<a class=\"repo\" href=\"https://github.com/robjcaskey/zcutils\">Repository ↗</a></nav></header><div class=\"layout\"><aside><p class=\"eyebrow\">The guides</p><nav aria-label=\"Guides\">{nav}</nav><div class=\"scope\">Executable laboratories.<br>Explicit production boundaries.</div></aside><main id=\"content\">{}<footer>Built from Markdown with a small Rust renderer. No client-side dependencies.</footer></main><nav class=\"toc\" aria-label=\"On this page\"><p class=\"eyebrow\">On this page</p>{toc}</nav></div></body></html>\n", escape(&doc.title), doc.html)));
    }
    // Never emit a partly validated site.
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    for (name, html) in rendered {
        fs::create_dir_all(output.join(name).parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(output.join(name), html).map_err(|e| e.to_string())?;
    }
    if !diskpipe {
        fs::write(output.join("day-2.html"), "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0;url=zccusan/day-2.html\"><title>zccusan day-two operations</title><a href=\"zccusan/day-2.html\">Continue to zccusan day-two operations</a><script>location.replace('zccusan/day-2.html'+location.hash)</script></html>").map_err(|e| e.to_string())?;
    }
    fs::copy(root.join("docs-site/site.css"), output.join("site.css"))
        .map_err(|e| e.to_string())?;
    println!(
        "DOCS_SITE_PASS pages={} output={}",
        pages.len(),
        output.display()
    );
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--site") if args.len() == 3 => site(Path::new(&args[1]), Path::new(&args[2]), false),
        Some("--diskpipe-site") if args.len() == 3 => {
            site(Path::new(&args[1]), Path::new(&args[2]), true)
        }
        Some("--check") if args.len() > 1 => {
            for file in &args[1..] {
                let source = fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
                render(&source, &safe_link).map_err(|e| format!("{file}: {e}"))?;
                println!("MARKDOWN_PASS {file}");
            }
            Ok(())
        }
        Some("--render") if args.len() == 3 => {
            let source = fs::read_to_string(&args[1]).map_err(|e| e.to_string())?;
            let doc = render(&source, &safe_link).map_err(|e| format!("{}: {e}", args[1]))?;
            fs::write(&args[2], doc.html).map_err(|e| e.to_string())
        }
        _ => Err(
            "usage: zc-md --check FILE... | --render INPUT OUTPUT | --site REPO_ROOT OUTPUT_DIR"
                .into(),
        ),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("zc-md: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> Result<Document> {
        render(s, &safe_link)
    }
    #[test]
    fn code_is_escaped() {
        let doc = parse("# Title\n\n```html\n<script>alert(1)</script>\n```\n").unwrap();
        assert!(doc.html.contains("&lt;script&gt;"));
        assert!(!doc.html.contains("<script>"));
    }
    #[test]
    fn dangerous_links_fail() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:bad",
            "//evil.example",
            "data:text/html,bad",
            "java&#58;script",
            "a\nb",
        ] {
            assert!(safe_link(url).is_err(), "{url}");
        }
        assert!(parse("# Title\n\n[x](javascript:alert(1))").is_err());
    }
    #[test]
    fn unsupported_and_unclosed_blocks_fail() {
        for source in [
            "# T\n\n```sh\necho hi",
            "# T\n\n<div>unsafe</div>",
            "# T\n\n> quote",
            "# T\n\nParagraph\n> quote",
            "# T\n\n![x](a.png)",
            "# T\n\n`open",
            "# T\n\n**open",
            "# T\n\n- Parent\n  - Nested",
        ] {
            assert!(parse(source).is_err(), "{source}");
        }
    }
    #[test]
    fn headings_have_unique_stable_anchors() {
        assert!(parse("# T\n\n### Skipped").is_err());
        assert!(parse("# T\n\n## Again\n\n## Again").is_err());
        assert!(parse("No title").is_err());
        assert!(
            parse("# T\n\n## Read & write")
                .unwrap()
                .html
                .contains("id=\"read--write\"")
        );
    }
    #[test]
    fn table_width_is_checked() {
        assert!(parse("# T\n\n| A | B |\n| --- | --- |\n| one |").is_err());
        assert!(
            parse("# T\n\n| A | B |\n| --- | --- |\n| **one** | `two` |")
                .unwrap()
                .html
                .contains("<strong>one</strong>")
        );
    }
    #[test]
    fn lists_and_wrapped_inline_content() {
        let doc = parse("# T\n\n1. First\n   continued with `code`\n2. [Second](https://example.com/a(b))\n\n- Item\n  continued").unwrap();
        assert!(doc.html.contains("<ol>"));
        assert!(doc.html.contains("<ul>"));
        assert!(doc.html.contains("https://example.com/a(b)"));
        assert!(doc.html.contains("First continued"));
        assert!(
            parse("# T\n\n3. Third")
                .unwrap()
                .html
                .contains("<ol start=\"3\">")
        );
    }
    #[test]
    fn local_links_cannot_escape_root() {
        assert!(normalized(Path::new("docs"), "../../secret").is_err());
        assert_eq!(
            normalized(Path::new("docs"), "../docs-site/index.md").unwrap(),
            Path::new("docs-site/index.md")
        );
    }
}
