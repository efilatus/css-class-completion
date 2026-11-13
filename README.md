# CSS Class Completion

An [OpenIDEA](https://github.com/l0ng-ai/openidea) plugin that provides CSS class name completion in HTML documents.

## Features

- **CSS class completion** — suggests class names while typing inside HTML `class` attributes.
- **Multiple stylesheet formats** — extracts class names from `.css`, `.scss`, and `.less` files.
- **Prefix matching** — filters suggestions based on the text already entered.
- **Multiple attribute formats** — supports single-quoted, double-quoted, and unquoted `class` values.
- **Lightweight caching** — caches discovered class names for 3 seconds to reduce repeated file scanning.

## How It Works

The plugin searches the current project for CSS, SCSS, and LESS files, extracts class names from their selectors, and offers matching names through the OpenIDEA completion provider.

It scans up to 200 files per stylesheet format and returns up to 500 matching suggestions.

## Requirements

- OpenIDEA 1.6 or later.
- The `project.read` capability must be granted so the plugin can read project files.

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

- Completion is available in HTML documents only.
- Class names are extracted from stylesheet selectors using a lightweight parser, not a full CSS parser.
- Dynamically generated class names from JavaScript or template expressions are not discovered.

## License

This project is licensed under the [MIT License](LICENSE).
