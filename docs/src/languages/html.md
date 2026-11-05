---
title: HTML
description: "Configure HTML language support in Zed, including language servers, formatting, and debugging."
---

# HTML

HTML support is available through the [HTML extension](https://github.com/zed-industries/zed/tree/main/extensions/html).

- Tree-sitter: [tree-sitter/tree-sitter-html](https://github.com/tree-sitter/tree-sitter-html)
- Language Server: [microsoft/vscode-html-languageservice](https://github.com/microsoft/vscode-html-languageservice)

This extension is automatically installed, but if you do not want to use it, you can add the following to your settings:

```json [settings]
{
  "auto_install_extensions": {
    "html": false
  }
}
```

## Formatting

By default Zed uses [Prettier](https://prettier.io/) for formatting HTML.

Configure formatting in Settings ({#kb zed::OpenSettings}) under Languages > HTML, or add to your settings file:

```json [settings]
  "languages": {
    "HTML": {
      "format_on_save": "off",
    }
  }
```

You can still trigger formatting manually with {#kb editor::Format} or by opening the [command palette](../command-palette.md) ({#kb command_palette::Toggle}) and selecting "Format Document".

### LSP Formatting

To use the `vscode-html-language-server` language server auto-formatting instead of Prettier, configure the formatter in Settings ({#kb zed::OpenSettings}) under Languages > HTML, or add to your settings file:

```json [settings]
  "languages": {
    "HTML": {
      "formatter": "language_server",
    }
  }
```

You can customize various [formatting options](https://code.visualstudio.com/docs/languages/html#_formatting) for `vscode-html-language-server` via your Zed `settings.json`:

```json [settings]
  "lsp": {
    "vscode-html-language-server": {
      "settings": {
        "html": {
          "format": {
            // Indent under <html> and <head> (default: false)
            "indentInnerHtml": true,
            // Disable formatting inside <svg> or <script>
            "contentUnformatted": "svg,script",
            // Add an extra newline before <div> and <p>
            "extraLiners": "div,p"
          }
        }
      }
    }
  }
```

## See also

- [CSS](./css.md)
- [JavaScript](./javascript.md)
- [TypeScript](./typescript.md)
