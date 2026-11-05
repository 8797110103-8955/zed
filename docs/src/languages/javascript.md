---
title: JavaScript
description: "Configure JavaScript language support in Zed, including language servers, formatting, and debugging."
---

# JavaScript

JavaScript support is available natively in Zed.

- Tree-sitter: [tree-sitter/tree-sitter-javascript](https://github.com/tree-sitter/tree-sitter-javascript)
- Language Server: [yioneko/vtsls](https://github.com/yioneko/vtsls)
- Alternate Language Server: [typescript-language-server/typescript-language-server](https://github.com/typescript-language-server/typescript-language-server)
- Debug Adapter: [vscode-js-debug](https://github.com/microsoft/vscode-js-debug)

## Code formatting

Formatting on save is disabled by default for JavaScript.

To turn `format-on-save` on, you can enable the setting globally or for `JavaScript` or `TypeScript` specifically.

```json [settings]
{
  "languages": {
    "JavaScript": {
      "format_on_save": "on"
    },
    "TypeScript": {
      "format_on_save": "on"
    }
  }
}
```

Many JavaScript projects use command-line code-formatting tools, such as [Prettier](https://prettier.io/).
You can use one of these tools by specifying an _external_ code formatter for JavaScript in your settings.
See [the configuration docs](../reference/all-settings.md) for more information.

For example, if you have Prettier installed and on your `PATH`, you can use it to format JavaScript files.

Configure formatting in Settings ({#kb zed::OpenSettings}) under Languages > JavaScript, or add to your settings file:

```json [settings]
{
  "languages": {
    "JavaScript": {
      "formatter": {
        "external": {
          "command": "prettier",
          "arguments": ["--stdin-filepath", "{buffer_path}"]
        }
      }
    }
  }
}
```

## JSX

Zed supports JSX syntax highlighting out of the box.

## JSDoc

Zed supports JSDoc syntax in JavaScript and TypeScript comments that match the JSDoc syntax.
Zed uses [tree-sitter/tree-sitter-jsdoc](https://github.com/tree-sitter/tree-sitter-jsdoc) for parsing and highlighting JSDoc.

## Debugging

Zed supports debugging JavaScript code out of the box with `vscode-js-debug`.
The following can be debugged without writing additional configuration:

- Tasks from `package.json`
- Tests written using several popular frameworks (Jest, Mocha, Vitest, Jasmine, Bun, Node)

Run {#action debugger::Start} ({#kb debugger::Start}) to see a contextual list of these predefined [debug tasks](../debugger.md).

> **Note:** Bun test is automatically detected when `@types/bun` is present in `package.json`.

> **Note:** Node test is automatically detected when `@types/node` is present in `package.json` (requires Node.js 20+).

As for all languages, configurations from `.vscode/launch.json` are also available for debugging in Zed.

If your use-case isn't covered by any of these, you can take full control by adding debug configurations to `.zed/debug.json`. See below for example configurations.

### Configuring JavaScript debug tasks

JavaScript debugging is more complicated than other languages because there are two different environments: Node.js and the browser. `vscode-js-debug` exposes a `type` field, that you can use to specify the environment, either `node` or `chrome`.

- [vscode-js-debug configuration documentation](https://github.com/microsoft/vscode-js-debug/blob/main/OPTIONS.md)

### Debug the current file with Node

```json [debug]
[
  {
    "adapter": "JavaScript",
    "label": "Debug JS file",
    "type": "node",
    "request": "launch",
    "program": "$ZED_FILE",
    "skipFiles": ["<node_internals>/**"]
  }
]
```

### Launch a web app in Chrome

```json [debug]
[
  {
    "adapter": "JavaScript",
    "label": "Debug app in Chrome",
    "type": "chrome",
    "request": "launch",
    "file": "$ZED_WORKTREE_ROOT/index.html",
    "webRoot": "$ZED_WORKTREE_ROOT",
    "console": "integratedTerminal",
    "skipFiles": ["<node_internals>/**"]
  }
]
```

## See also

- [Yarn documentation](./yarn.md) for a walkthrough of configuring your project to use Yarn.
- [TypeScript documentation](./typescript.md)
