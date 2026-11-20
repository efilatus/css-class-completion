
use openidea_plugin::{
    DocumentChanged, Error, Event, Invocation, Plugin, Position, Range, host, register_plugin,
};
use openidea_plugin::providers::{
    CompletionItem, CompletionKind, Document, Providers,
};
use openidea_plugin::text::{offset, position, position_utf16};

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: Duration = Duration::from_secs(15);
const MAX_FILES: u32 = 200;
const MAX_ITEMS: usize = 500;
const MRU_SIZE: usize = 5;
const OFFER_TTL_SECS: u64 = 10;
const MAX_PENDING_OFFERS: usize = 16;
const FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
const FNV1A64_PRIME: u64 = 0x100000001b3;
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

#[derive(Clone)]
struct Offer {
    labels: Vec<String>,
    /// Snapshot of the completion that produced this offer. Matching uses the
    /// inserted text, not these fields.
    #[allow(dead_code)]
    replace: Range,
    #[allow(dead_code)]
    version: u64,
    created_unix: u64,
    /// Insertion order, so same-second offers still evict oldest-first.
    seq: u64,
}

#[derive(Default)]
pub struct Main {
    cache: Option<(String, Instant, Vec<String>)>,
    /// Project root → recent classes, index 0 is the newest. Loaded on first use.
    mru: HashMap<String, Vec<String>>,
    offers: HashMap<String, Offer>,
    next_offer: u64,
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

        let matched: Vec<String> = classes.into_iter().filter(|name| name.starts_with(typed)).collect();
        let root = host::project_of(&doc.path).and_then(|project| project.root().ok());
        let mru = match root.as_deref() {
            Some(root) => self.ensure_mru(root).cloned().unwrap_or_default(),
            None => Vec::new(),
        };
        let items: Vec<CompletionItem> = rank_matches(&matched, &mru)
            .into_iter()
            .take(MAX_ITEMS)
            .enumerate()
            .map(|(index, name)| {
                let detail = if mru.iter().any(|recent| recent == &name) {
                    "CSS class · recently used"
                } else {
                    "CSS class"
                };
                CompletionItem {
                    replace: Some(replace),
                    detail: Some(detail.to_string()),
                    sort_text: Some(format!("{index:04}")),
                    ..CompletionItem::new(&name, CompletionKind::Reference)
                }
            })
            .collect();

        let labels = items.iter().map(|item| item.label.clone()).collect();
        let key = match root.as_deref() {
            Some(root) => offer_key(&doc.path, root),
            None => normalize_path(&doc.path),
        };
        self.remember_offer(
            key,
            Offer {
                labels,
                replace,
                version: doc.version,
                created_unix: now_unix(),
                seq: 0,
            },
        );
        items
    }
}

impl Plugin for Main {
    fn activate(&mut self, _ctx: &openidea_plugin::Context) -> Result<(), Error> {
        host::subscribe_paths(
            "document-changed",
            &[
                "**/*.html",
                "**/*.vue",
                "**/*.php",
                "**/*.twig",
                "**/*.js",
                "**/*.jsx",
                "**/*.ts",
                "**/*.tsx",
                "**/*.svelte",
            ],
        )
    }

    fn run_command(
        &mut self,
        id: &str,
        _ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        Err(format!("unknown command {id}").into())
    }

    fn handle_events(&mut self, events: &[Event], ctx: &Invocation) {
        for event in events {
            let Event::DocumentChanged(change) = event else {
                continue;
            };
            self.note_selection(change, ctx);
        }
    }
}

register_plugin!(Main, providers);

impl Main {
    /// Loads this project's MRU on first use. `None` when storage cannot be read,
    /// so a later save does not overwrite data we failed to load.
    fn ensure_mru(&mut self, root: &str) -> Option<&mut Vec<String>> {
        let root = normalize_root(root);
        if !self.mru.contains_key(&root) {
            let loaded = match host::storage::get(&mru_key(&root)) {
                Ok(Some(raw)) => parse_mru(&raw),
                Ok(None) => Vec::new(),
                Err(_) => return None,
            };
            self.mru.insert(root.clone(), loaded);
        }
        self.mru.get_mut(&root)
    }

    fn remember_offer(&mut self, path: String, mut offer: Offer) {
        self.next_offer = self.next_offer.saturating_add(1);
        offer.seq = self.next_offer;
        self.offers.insert(path, offer);
        while self.offers.len() > MAX_PENDING_OFFERS {
            let oldest = self.offers.iter().min_by_key(|(_, offer)| offer.seq).map(|(path, _)| path.clone());
            let Some(oldest) = oldest else { break };
            self.offers.remove(&oldest);
        }
    }

    fn find_offer(&self, path: &str, ctx: &Invocation) -> Option<Offer> {
        if let Some(offer) = self.offers.get(path).cloned() {
            return Some(offer);
        }
        let normalized = normalize_path(path);
        if let Some(offer) = self.offers.get(&normalized).cloned() {
            return Some(offer);
        }
        let root = resolve_root(path, ctx)?;
        self.offers.get(&offer_key(path, &root)).cloned()
    }

    fn note_selection(&mut self, change: &DocumentChanged, ctx: &Invocation) {
        let Some(offer) = self.find_offer(&change.path, ctx) else {
            return;
        };
        let now = now_unix();
        if !offer_is_fresh(&offer, now) {
            return;
        }
        if !change.changes.iter().any(|edit| label_eq(&offer, &edit.insert)) {
            return;
        }
        let Some(root) = resolve_root(&change.path, ctx) else {
            return;
        };
        let Some(doc) = read_document(&change.path, ctx) else {
            return;
        };

        let mut shift = 0i64;
        let mut picked = Vec::new();
        for edit in &change.changes {
            let deleted = if edit.from <= edit.to { edit.to - edit.from } else { 0 };
            let end = if edit.from <= edit.to {
                insertion_end_byte(&doc.text, edit.from, &edit.insert, shift)
            } else {
                None
            };
            shift += i64::from(utf16_len(&edit.insert)) - i64::from(deleted);
            let Some(byte) = end else { continue };
            if selection_counts(&offer, now, &edit.insert, &doc.text, byte) {
                picked.push(edit.insert.clone());
            }
        }
        if picked.is_empty() {
            return;
        }
        let Some(mru) = self.ensure_mru(&root) else {
            return;
        };
        for class in &picked {
            mru_update(mru, class);
        }
        let saved = mru.clone();
        save_mru(&root, &saved);
    }
}

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

        let value = tag.get(value_start..value_end)?;
        let local_cursor = cursor - (tag_start + 1 + value_start);

        let before_cursor = value.get(..local_cursor)?;
        let after_cursor = value.get(local_cursor..)?;

        // Начало класса: прыгаем на ПОЛНУЮ длину пробела (len_utf8), а не на 1 байт.
        // Иначе на U+3000 / NBSP попадаем в середину символа и паникуем.
        let token_start = before_cursor
            .char_indices()
            .rev()
            .find(|&(_, c)| c.is_whitespace())
            .map_or(0, |(p, c)| p + c.len_utf8());

        // Конец класса: find возвращает начало пробела — это уже граница символа.
        let token_end = after_cursor
            .find(char::is_whitespace)
            .map_or(value.len(), |p| local_cursor + p);

        let absolute_start = tag_start + 1 + value_start + token_start;
        let absolute_end = tag_start + 1 + value_start + token_end;

        let typed = value.get(token_start..local_cursor)?;
        return Some((absolute_start, absolute_end, typed));
    }

    None
}

fn mru_update(mru: &mut Vec<String>, class: &str) {
    mru.retain(|existing| existing != class);
    mru.insert(0, class.to_string());
    mru.truncate(MRU_SIZE);
}

/// MRU hits first (newest first), then the remaining names in their current order.
fn rank_matches(candidates: &[String], mru: &[String]) -> Vec<String> {
    let mut ranked: Vec<String> = mru.iter().filter(|class| candidates.contains(*class)).cloned().collect();
    ranked.extend(candidates.iter().filter(|class| !mru.contains(*class)).cloned());
    ranked
}

fn offer_is_fresh(offer: &Offer, now: u64) -> bool {
    now.saturating_sub(offer.created_unix) <= OFFER_TTL_SECS
}

fn label_eq(offer: &Offer, inserted: &str) -> bool {
    offer.labels.iter().any(|label| label == inserted)
}

/// Exact offered label, still fresh, and the caret after the insert sits in `class`.
fn selection_counts(offer: &Offer, now: u64, inserted: &str, text: &str, end_byte: usize) -> bool {
    offer_is_fresh(offer, now) && label_eq(offer, inserted) && class_value_prefix(text, end_byte).is_some()
}

/// Byte offset of the end of `insert` in the document after the edit.
/// `from` is a UTF-16 offset in the pre-edit text; `shift_utf16` is the
/// UTF-16 delta of earlier edits in the same change set.
fn insertion_end_byte(text: &str, from: u32, insert: &str, shift_utf16: i64) -> Option<usize> {
    let end = i64::from(from) + shift_utf16 + i64::from(utf16_len(insert));
    if end < 0 || end > i64::from(u32::MAX) {
        return None;
    }
    let pos = position_utf16(text, end as u32)?;
    offset(text, pos)
}

fn utf16_len(text: &str) -> u32 {
    text.chars().map(|ch| ch.len_utf16() as u32).sum()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A64_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    hash
}

fn mru_key(root: &str) -> String {
    format!("mru:v1:{:016x}", fnv1a64(normalize_root(root).as_bytes()))
}

fn serialize_mru(mru: &[String]) -> String {
    mru.iter().take(MRU_SIZE).cloned().collect::<Vec<_>>().join("\n")
}

fn parse_mru(raw: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in raw.split(['\n', '\r']) {
        let name = line.trim();
        if !valid_class_name(name) || names.iter().any(|existing| existing == name) {
            continue;
        }
        names.push(name.to_string());
        if names.len() == MRU_SIZE {
            break;
        }
    }
    names
}

fn valid_class_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_digit() || !(first.is_alphanumeric() || first == '_' || first == '-') {
        return false;
    }
    chars.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
}

fn save_mru(root: &str, mru: &[String]) {
    let _ = host::storage::set(&mru_key(root), &serialize_mru(mru));
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn normalize_root(root: &str) -> String {
    normalize_path(root).trim_end_matches('/').to_string()
}

fn is_absolute_path(path: &str) -> bool {
    let path = normalize_path(path);
    path.starts_with('/')
        || path.as_bytes().get(1) == Some(&b':') && path.as_bytes().get(2) == Some(&b'/')
}

fn offer_key(path: &str, root: &str) -> String {
    let path = normalize_path(path);
    let root = normalize_root(root);
    if is_absolute_path(&path) {
        path
    } else {
        format!("{root}/{}", path.trim_start_matches('/'))
    }
}

fn resolve_root(path: &str, ctx: &Invocation) -> Option<String> {
    if let Some(project) = host::project_of(path)
        && let Ok(root) = project.root()
    {
        return Some(normalize_root(&root));
    }
    ctx.project().and_then(|project| project.root().ok()).map(|root| normalize_root(&root))
}

fn read_document(path: &str, ctx: &Invocation) -> Option<openidea_plugin::Document> {
    if let Some(project) = host::project_of(path)
        && let Ok(doc) = project.document(path)
    {
        return Some(doc);
    }
    ctx.project()?.document(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Тесты Cursor: языки, исключение папок, парсинг селекторов =====

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

    // ===== Мои тесты: границы UTF-8 при разных пробелах (регрессия из репорта) =====

    fn check_btn_token(text: &str, expect_token: &str) {
        let cursor = text.find("btn").unwrap() + "btn".len();
        let (s, e, typed) = class_value_prefix(text, cursor).unwrap();
        assert_eq!(typed, "btn");
        assert_eq!(&text[s..e], expect_token);
    }

    #[test]
    fn ascii_space() {
        check_btn_token(r#"<div class="other btn"></div>"#, "btn");
    }

    #[test]
    fn ideographic_space_u3000() {
        check_btn_token("<div class=\"other\u{3000}btn\"></div>", "btn");
    }

    #[test]
    fn nbsp_u00a0() {
        check_btn_token("<div class=\"other\u{00a0}btn\"></div>", "btn");
    }

    #[test]
    fn mid_token_replacement() {
        check_btn_token(r#"<div class="other btn-primary"></div>"#, "btn-primary");
    }

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|name| (*name).to_string()).collect()
    }

    fn sample_mru() -> Vec<String> {
        names(&["box__blue", "btn", "container", "card", "text-muted"])
    }

    #[test]
    fn rank_puts_recent_prefix_matches_first_without_hiding_the_rest() {
        let mru = sample_mru();
        assert_eq!(
            rank_matches(&names(&["box", "box__blue", "box__border"]), &mru),
            names(&["box__blue", "box", "box__border"])
        );
        assert_eq!(
            rank_matches(&names(&["badge", "banner", "box", "box__blue", "box__border", "btn", "button"]), &mru),
            names(&["box__blue", "btn", "badge", "banner", "box", "box__border", "button"])
        );
        assert_eq!(rank_matches(&names(&["xyz", "xyz-a"]), &mru), names(&["xyz", "xyz-a"]));
        assert_eq!(
            rank_matches(
                &names(&["badge", "box", "box__blue", "btn", "card", "container", "text-muted", "zebra"]),
                &mru
            ),
            names(&["box__blue", "btn", "container", "card", "text-muted", "badge", "box", "zebra"])
        );
        assert_eq!(
            rank_matches(&names(&["box", "card"]), &names(&["gone", "box"])),
            names(&["box", "card"])
        );
    }

    #[test]
    fn mru_update_moves_repeat_to_front_and_drops_the_oldest() {
        let mut mru = sample_mru();
        mru_update(&mut mru, "btn");
        assert_eq!(mru, names(&["btn", "box__blue", "container", "card", "text-muted"]));
        mru_update(&mut mru, "badge");
        assert_eq!(mru, names(&["badge", "btn", "box__blue", "container", "card"]));
        assert!(!mru.contains(&"text-muted".to_string()));
    }

    fn offer(labels: &[&str], created_unix: u64) -> Offer {
        Offer {
            labels: names(labels),
            replace: Range::default(),
            version: 1,
            created_unix,
            seq: 0,
        }
    }

    #[test]
    fn offer_matching_accepts_only_a_fresh_exact_label_inside_class() {
        let html = r#"<div class="box__blue">"#;
        let at = html.rfind('"').unwrap();
        let fresh = offer(&["box__blue", "btn"], 100);
        assert!(selection_counts(&fresh, 100, "box__blue", html, at));
        assert!(selection_counts(&fresh, 110, "box__blue", html, at), "age of 10s still counts");
        assert!(!selection_counts(&fresh, 100, "b", html, at));
        assert!(!selection_counts(&fresh, 100, "pasted", html, at));
        assert!(!selection_counts(&fresh, 111, "box__blue", html, at));

        let outside = r#"<div class="ok"><p>box__blue</p>"#;
        let body = outside.rfind("box__blue").unwrap() + "box__blue".len();
        assert!(!selection_counts(&fresh, 100, "box__blue", outside, body));
    }

    #[test]
    fn insertion_end_uses_utf16_offsets_in_the_edited_text() {
        let text = "<div title=\"😀\" class=\"box__blue\">";
        let start = text.find("box__blue").unwrap();
        let from = utf16_len(&text[..start]);
        let end = insertion_end_byte(text, from, "box__blue", 0).unwrap();
        assert_eq!(&text[start..end], "box__blue");
        assert!(class_value_prefix(text, end).is_some());
        assert_ne!(from as usize, start, "the emoji shifts bytes away from UTF-16");
    }

    #[test]
    fn mru_storage_roundtrip_skips_broken_lines_and_keeps_five() {
        let names = names(&["box__blue", "btn", "container", "card", "text-muted"]);
        assert_eq!(parse_mru(&serialize_mru(&names)), names);
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(
            parse_mru("box__blue\n\n   \nnot a class\n!!!\nbtn\ncontainer\ncard\ntext-muted\nextra\n"),
            names
        );
        assert!(mru_key("/project").starts_with("mru:v1:"));
        assert_ne!(mru_key("/project"), mru_key("/other"));
    }

    #[test]
    fn pending_offers_keep_the_sixteen_newest() {
        let mut main = Main::default();
        for index in 0..MAX_PENDING_OFFERS + 1 {
            main.remember_offer(
                format!("/project/f{index}.html"),
                offer(&["box"], index as u64),
            );
        }
        assert_eq!(main.offers.len(), MAX_PENDING_OFFERS);
        assert!(!main.offers.contains_key("/project/f0.html"));
        assert!(main.offers.contains_key(&format!("/project/f{MAX_PENDING_OFFERS}.html")));
    }

    #[test]
    fn picked_class_is_stored_and_ranked_after_restart() {
        let mock = openidea_plugin::mock::MockHost::install();
        mock.add_file("src/app.css", ".box { color: red } .box__blue { } .box__border { } .btn { }");
        let mut plugin = Main::default();
        plugin.activate(&openidea_plugin::Context::default()).unwrap();
        let globs = mock.path_subscriptions()["document-changed"].clone();
        assert_eq!(
            globs,
            [
                "**/*.html",
                "**/*.vue",
                "**/*.php",
                "**/*.twig",
                "**/*.js",
                "**/*.jsx",
                "**/*.ts",
                "**/*.tsx",
                "**/*.svelte",
            ]
        );

        let before = r#"<div class="box">"#;
        let after = r#"<div class="box__blue">"#;
        mock.open_editor("/project/index.html", "html", after, &[]);
        let doc = Document {
            path: "/project/index.html".into(),
            language: "html".into(),
            version: 1,
            text: before.into(),
        };
        let cursor = before.find("box").unwrap() + "box".len();
        let items = plugin.completion(&doc, position(before, cursor), None);
        assert_eq!(
            items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
            ["box", "box__blue", "box__border"]
        );
        assert_eq!(items[0].sort_text.as_deref(), Some("0000"));
        assert_eq!(items[0].detail.as_deref(), Some("CSS class"));
        assert!(mock.storage().is_empty());

        let start = after.find("box__blue").unwrap();
        let from = utf16_len(&after[..start]);
        plugin.handle_events(
            &[Event::DocumentChanged(DocumentChanged {
                path: "index.html".into(),
                base_version: 0,
                version: mock.editor_version(),
                changes: vec![openidea_plugin::DocumentTextChange {
                    from,
                    to: from + utf16_len("box"),
                    insert: "box__blue".into(),
                }],
            })],
            &mock.invocation(),
        );
        assert_eq!(mock.storage().get(&mru_key("/project")).map(String::as_str), Some("box__blue"));

        plugin.handle_events(
            &[Event::DocumentChanged(DocumentChanged {
                path: "index.html".into(),
                base_version: 1,
                version: mock.editor_version(),
                changes: vec![openidea_plugin::DocumentTextChange {
                    from,
                    to: from,
                    insert: "b".into(),
                }],
            })],
            &mock.invocation(),
        );
        assert_eq!(mock.storage().get(&mru_key("/project")).map(String::as_str), Some("box__blue"));

        plugin.remember_offer(
            "/project/index.html".into(),
            Offer {
                labels: names(&["box__blue", "btn"]),
                replace: Range::default(),
                version: 1,
                created_unix: 0,
                seq: 0,
            },
        );
        plugin.handle_events(
            &[Event::DocumentChanged(DocumentChanged {
                path: "index.html".into(),
                base_version: 1,
                version: mock.editor_version(),
                changes: vec![openidea_plugin::DocumentTextChange {
                    from,
                    to: from,
                    insert: "btn".into(),
                }],
            })],
            &mock.invocation(),
        );
        assert_eq!(
            mock.storage().get(&mru_key("/project")).map(String::as_str),
            Some("box__blue"),
            "an offer older than 10s does not count"
        );

        let mut restarted = Main::default();
        let again = restarted.completion(&doc, position(before, cursor), None);
        assert_eq!(
            again.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
            ["box__blue", "box", "box__border"]
        );
        assert_eq!(again[0].detail.as_deref(), Some("CSS class · recently used"));
        assert_eq!(again[1].detail.as_deref(), Some("CSS class"));
        assert!(again.iter().all(|item| item.label != "btn"));
    }

    #[test]
    fn two_projects_store_independent_mru_lists() {
        let mock = openidea_plugin::mock::MockHost::install();
        mock.set_project_root(Some("/proj-a"));
        mock.add_project("/proj-b");
        mock.add_file("src/app.css", ".box__blue { } .btn { }");

        let mut plugin = Main::default();
        select_class(&mock, &mut plugin, "/proj-a/index.html", "box__blue");
        mock.add_project("/proj-a");
        mock.set_project_root(Some("/proj-b"));
        select_class(&mock, &mut plugin, "/proj-b/page.html", "btn");

        assert_eq!(mock.storage().get(&mru_key("/proj-a")).map(String::as_str), Some("box__blue"));
        assert_eq!(mock.storage().get(&mru_key("/proj-b")).map(String::as_str), Some("btn"));
    }

    fn select_class(mock: &openidea_plugin::mock::MockHost, plugin: &mut Main, path: &str, class: &str) {
        let text = format!(r#"<div class="{class}">"#);
        mock.open_editor(path, "html", &text, &[]);
        let doc = Document {
            path: path.into(),
            language: "html".into(),
            version: mock.editor_version(),
            text: text.clone(),
        };
        let cursor = text.find(class).unwrap() + class.len();
        let _ = plugin.completion(&doc, position(&text, cursor), None);
        let from = utf16_len(&text[..text.find(class).unwrap()]);
        plugin.handle_events(
            &[Event::DocumentChanged(DocumentChanged {
                path: path.into(),
                base_version: 0,
                version: mock.editor_version(),
                changes: vec![openidea_plugin::DocumentTextChange {
                    from,
                    to: from,
                    insert: class.into(),
                }],
            })],
            &mock.invocation(),
        );
    }
}
