# CSS Class Completion

An OpenIDEA plugin that provides CSS class name completion from your project stylesheets.

Version 0.2.0.

## Features

- **CSS class completion** — suggests class names while typing inside `class` attributes.
- **Recent selections first** — the last 5 classes you picked from completion stay on top, per project, persisted across restarts.
- **Multiple languages** — works in HTML, Vue, PHP, Twig, JavaScript, TypeScript, and Svelte documents.
- **Multiple stylesheet formats** — extracts class names from `.css`, `.scss`, and `.less` files, including CSS Modules (`*.module.css` / `*.module.scss` / `*.module.less`).
- **Prefix matching** — filters suggestions based on the text already entered.
- **Smart token replacement** — replaces the whole class name under the cursor, even when editing in the middle of a token.
- **Multiple attribute formats** — supports single-quoted, double-quoted, and unquoted `class` values.
- **Unicode-safe** — handles non-ASCII whitespace (e.g. NBSP, U+3000) as class separators without panics.
- **Lightweight caching** — caches discovered class names for 15 seconds to reduce repeated file scanning.

## How It Works

The plugin searches the current project for CSS, SCSS, and LESS files, extracts class names from their selectors, and offers matching names through the OpenIDEA completion provider.

- Scans up to **200 files per glob pattern** and returns up to **500 matching suggestions**.
- Skips `node_modules`, `dist`, `build`, `.git`, `target`, and `vendor` directories.
- Additionally reads well-known framework stylesheets (`tailwind.css`, `styles.css`, `bootstrap.min.css`) directly from `src/`, `public/`, and `assets/`.
- The selector parser ignores comments and string literals, so text like `content: ".not-a-class"` never pollutes suggestions.

## Requirements

- OpenIDEA 1.6 or later.
- The `project.read`, `editor.read`, `events`, and `storage` capabilities must be granted. OpenIDEA asks again when the plugin is updated.

## Development

### Build and test

```bash
cargo test
openidea-plugin build
openidea-plugin check
```

### Link the plugin locally

From the project directory:

```bash
openidea-plugin link
```

This loads the plugin folder in OpenIDEA. You can manage linked plugins in **Settings → Plugins**.

## Limitations

- Only a class picked from the completion list changes suggestion order. Typing a class by hand or pasting it does not.
- Recent selections stay on this machine. The plugin does not write them into project files.
- Completion triggers inside `class` attributes only — JSX `className`, Vue `:class` bindings, and `classList.add()` calls are not supported yet.
- Class names are extracted using a lightweight parser, not a full CSS parser: SCSS nesting with `&` (e.g. BEM `&__element`) is not expanded into full class names.
- Tailwind utility classes are only suggested if they exist in a stylesheet — they are not generated on the fly.
- Dynamically generated class names from JavaScript or template expressions are not discovered.

## License

This project is licensed under the MIT License.
