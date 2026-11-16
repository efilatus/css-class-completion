
use openidea_plugin::{
    Error, Invocation, Plugin, Position, Range, host, register_plugin,
};
use openidea_plugin::providers::{
    CompletionItem, CompletionKind, Document, Providers,
};
use openidea_plugin::text::{offset, position};

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(15);
const MAX_FILES: u32 = 200;
/// Index hits to inspect per pattern. Higher than `MAX_FILES` so excluded
/// directories do not consume the read quota.
const FIND_LIMIT: u32 = 4_000;

const COMPLETION_LANGUAGES: &[&str] = &[
    "html",
    "vue",
    "php",
    "twig",
    "javascript",
    "typescript",
    "svelte",
];

const STYLESHEET_GLOBS: &[&str] = &[
    "**/*.css",
    "**/*.scss",
    "**/*.less",
    "**/*.module.css",
    "**/*.module.scss",
    "**/*.module.less",
];

const EXCLUDED_DIRS: &[&str] = &["node_modules", "dist", "build", ".git", "target", "vendor"];

const FRAMEWORK_DIRS: &[&str] = &["src", "public", "assets"];
const FRAMEWORK_FILES: &[&str] = &["tailwind.css", "styles.css", "bootstrap.min.css"];

#[derive(Default)]
pub struct Main {
    cache: Option<(String, Instant, Vec<String>)>,
}

impl Main {
    fn load_classes(&mut self, doc: &Document) -> Vec<String> {
        let Some(project) = host::project_of(&doc.path) else {
            return Vec::new();
        };

        let Ok(root) = project.root() else {
            return Vec::new();
        };

        if let Some((cached_root, time, classes)) = &self.cache {
            if cached_root == &root && time.elapsed() < CACHE_TTL {
                return classes.clone();
            }
        }

        let mut classes = BTreeSet::new();
        let mut seen = BTreeSet::new();

        for pattern in STYLESHEET_GLOBS {
            let Ok(files) = project.find_files(pattern, FIND_LIMIT) else {
                continue;
            };

            let mut kept = 0u32;
            for file in files {
                if path_is_excluded(&file, &root) || !seen.insert(file.clone()) {
                    continue;
                }

                let Ok(content) = project.read_file(&file) else {
                    continue;
                };

                extract_classes(&content, &mut classes);
                kept += 1;
                if kept >= MAX_FILES {
                    break;
                }
            }
        }

        load_framework_classes(&project, &mut classes);

        let result: Vec<String> = classes.into_iter().collect();

        self.cache = Some((
            root,
            Instant::now(),
            result.clone(),
        ));

        result
    }
}

impl Providers for Main {
    fn completion(
        &mut self,
        doc: &Document,
        at: Position,
        _trigger: Option<&str>,
    ) -> Vec<CompletionItem> {
        if !completion_language(&doc.language) {
            return Vec::new();
        }

        let Some(cursor) = offset(&doc.text, at) else {
            return Vec::new();
        };

        let Some((start, end, typed)) = class_value_prefix(&doc.text, cursor) else {
            return Vec::new();
        };

        let classes = self.load_classes(doc);

        let replace = Range::new(
            position(&doc.text, start),
            position(&doc.text, end),
        );

        classes
        .into_iter()
        .filter(|name| name.starts_with(typed))
        .take(500)
        .map(|name| CompletionItem {
            replace: Some(replace),
            detail: Some("CSS class".into()),
            ..CompletionItem::new(&name, CompletionKind::Reference)
        })
        .collect()
    }
}

impl Plugin for Main {
    fn run_command(
        &mut self,
        id: &str,
        _ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        Err(format!("unknown command {id}").into())
    }
}

register_plugin!(Main, providers);

fn completion_language(language: &str) -> bool {
    COMPLETION_LANGUAGES.contains(&language)
}

/// True when any path segment inside the project is a directory we refuse to scan.
fn path_is_excluded(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches(['/', '\\']);
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .trim_start_matches(['/', '\\']);

    relative
        .split(['/', '\\'])
        .any(|part| EXCLUDED_DIRS.contains(&part))
}

/// Reads a few well-known framework stylesheets directly, without walking `node_modules`.
fn load_framework_classes(project: &host::Project, classes: &mut BTreeSet<String>) {
    for dir in FRAMEWORK_DIRS {
        for name in FRAMEWORK_FILES {
            let path = format!("{dir}/{name}");
            let Ok(content) = project.read_file(&path) else {
                continue;
            };
            extract_classes(&content, classes);
        }
    }
}

/// Извлекает CSS-классы из селекторов.
fn extract_classes(css: &str, output: &mut BTreeSet<String>) {
    let bytes = css.as_bytes();
    let mut segment_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len()
                    && !(bytes[i] == b'*' && bytes[i + 1] == b'/')
                    {
                        i += 1;
                    }
                i = (i + 2).min(bytes.len());
            }

            b'"' | b'\'' => {
                let quote = bytes[i];
                i += 1;

                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else if bytes[i] == quote {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }

            b'{' => {
                if let Some(segment) = css.get(segment_start..i) {
                    collect_selector_classes(segment, output);
                }
                segment_start = i + 1;
                i += 1;
            }

            b'}' => {
                segment_start = i + 1;
                i += 1;
            }

            _ => i += 1,
        }
    }
}

fn collect_selector_classes(
    selector: &str,
    output: &mut BTreeSet<String>,
) {
    let chars: Vec<(usize, char)> = selector.char_indices().collect();

    let mut i = 0;

    while i < chars.len() {
        if chars[i].1 != '.' {
            i += 1;
            continue;
        }

        let next = i + 1;

        if next >= chars.len() {
            break;
        }

        let first = chars[next].1;

        if first.is_ascii_digit()
            || !(first.is_alphanumeric() || first == '_' || first == '-')
        {
            i += 1;
            continue;
        }

        let start = chars[next].0;
        let mut end = start + first.len_utf8();
        i = next + 1;

        while i < chars.len() {
            let c = chars[i].1;

            if c.is_alphanumeric() || c == '_' || c == '-' {
                end = chars[i].0 + c.len_utf8();
                i += 1;
            } else {
                break;
            }
        }

        if let Some(name) = selector.get(start..end) {
            output.insert(name.to_string());
        }
    }
}

/// Проверяет, находится ли курсор внутри значения атрибута class.
/// Проверяет, находится ли курсор внутри значения атрибута class.
/// Возвращает начало и конец текущего класса и префикс до курсора.
fn class_value_prefix(
    text: &str,
    cursor: usize,
) -> Option<(usize, usize, &str)> {
    let before = text.get(..cursor)?;

    let tag_start = before.rfind('<')?;

    if before[tag_start + 1..].starts_with('/')
        || before[tag_start + 1..].starts_with('!')
        || before[tag_start + 1..].starts_with('?')
    {
        return None;
    }

    let bytes = text.as_bytes();

    // Ищем конец открывающего тега, учитывая кавычки.
    let mut quote = None;
    let mut tag_end = text.len();
    let mut i = tag_start + 1;

    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                if quote == Some(bytes[i]) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(bytes[i]);
                }
            }
            b'>' if quote.is_none() => {
                tag_end = i;
                break;
            }
            _ => {}
        }

        i += 1;
    }

    if cursor > tag_end {
        return None;
    }

    let tag = &text[tag_start + 1..tag_end];
    let bytes = tag.as_bytes();

    let mut i = 0;

    // Пропускаем имя тега.
    while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'/'
        {
            i += 1;
        }

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i >= bytes.len() || bytes[i] == b'/' {
            break;
        }

        let name_start = i;

        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'/' | b'>')
            {
                i += 1;
            }

        if i == name_start {
            i += 1;
            continue;
        }

        let is_class = tag[name_start..i].eq_ignore_ascii_case("class");

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }

        i += 1;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i >= bytes.len() {
            break;
        }

        let (value_start, value_end);

        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;

            value_start = i;

            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }

            value_end = i;

            if i < bytes.len() {
                i += 1;
            }
        } else {
            value_start = i;

            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }

            value_end = i;
        }

        if !is_class {
            continue;
        }

        // Курсор должен находиться именно внутри значения class.
        if cursor < tag_start + 1 + value_start
            || cursor > tag_start + 1 + value_end
        {
            return None;
        }

        let value = &tag[value_start..value_end];
        let local_cursor = cursor - (tag_start + 1 + value_start);

        // Находим начало текущего класса.
        let token_start = value[..local_cursor]
            .rfind(char::is_whitespace)
            .map_or(0, |p| p + 1);

        // Находим конец текущего класса.
        let token_end = value[local_cursor..]
            .find(char::is_whitespace)
            .map_or(value.len(), |p| local_cursor + p);

        let absolute_start = tag_start + 1 + value_start + token_start;
        let absolute_end = tag_start + 1 + value_start + token_end;

        return Some((
            absolute_start,
            absolute_end,
            &value[token_start..local_cursor],
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_languages_cover_markup_and_script_hosts() {
        for language in [
            "html",
            "vue",
            "php",
            "twig",
            "javascript",
            "typescript",
            "svelte",
        ] {
            assert!(completion_language(language), "{language}");
        }
        assert!(!completion_language("css"));
        assert!(!completion_language("HTML"));
    }

    #[test]
    fn excluded_dirs_match_path_segments_inside_the_project_only() {
        let root = "/work/target";
        assert!(!path_is_excluded("/work/target/src/app.css", root));
        assert!(!path_is_excluded(
            "/work/target/src/Button.module.css",
            root
        ));
        assert!(path_is_excluded(
            "/work/target/node_modules/pkg/index.css",
            root
        ));
        assert!(path_is_excluded("/work/target/dist/app.css", root));
        assert!(path_is_excluded("/work/target/build/app.css", root));
        assert!(path_is_excluded("/work/target/.git/x.css", root));
        assert!(path_is_excluded("/work/target/target/app.css", root));
        assert!(path_is_excluded("/work/target/vendor/lib.css", root));
        assert!(path_is_excluded(
            r"C:\work\app\node_modules\pkg\index.css",
            r"C:\work\app"
        ));
        assert!(!path_is_excluded(
            "/work/target/src/dist-utils/app.css",
            root
        ));
    }

    #[test]
    fn extract_classes_reads_selectors_and_ignores_declaration_text() {
        let mut classes = BTreeSet::new();
        extract_classes(
            r#"
            .button, .button-primary { color: red; }
            .card::before { content: ".not-a-class"; /* .inside-block */ }
            "#,
            &mut classes,
        );
        assert!(classes.contains("button"));
        assert!(classes.contains("button-primary"));
        assert!(classes.contains("card"));
        assert!(!classes.contains("not-a-class"));
        assert!(!classes.contains("inside-block"));
    }

    #[test]
    fn class_value_prefix_still_returns_the_token_under_the_cursor() {
        let text = r#"<div class="btn primary">"#;
        let cursor = text.find("primary").unwrap() + "pri".len();
        let (start, end, typed) = class_value_prefix(text, cursor).unwrap();
        assert_eq!(typed, "pri");
        assert_eq!(&text[start..end], "primary");
    }
}
