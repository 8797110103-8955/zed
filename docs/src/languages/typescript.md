---
title: TypeScript
description: "Configure TypeScript language support in Zed, including language servers, formatting, and debugging."
---

# TypeScript

TypeScript and TSX support are available natively in Zed.

- Tree-sitter: [tree-sitter/tree-sitter-typescript](https://github.com/tree-sitter/tree-sitter-typescript)
- Language Server: TypeScript 7 native language server (`typescript-ls`, runs `tsc --lsp --stdio`)
- Debug Adapter: [vscode-js-debug](https://github.com/microsoft/vscode-js-debug)

## Language server

Zed uses the TypeScript 7 native language server, `typescript-ls`, for TypeScript, TSX, and JavaScript files.

- If the project has `typescript` 7 or newer installed (the nearest `node_modules/typescript`, up to the repository root), Zed runs the project's own compiler.
- If the project has an older TypeScript, Zed shows a one-time notification and uses its bundled TypeScript 7. TypeScript 6 and older are not supported; upgrade the project to `typescript@7` so diagnostics match your build.
- Otherwise Zed installs and uses its bundled TypeScript 7.

TypeScript 7 does not load tsserver plugins, so language servers that depend on them (such as Vue's hybrid mode) are not supported.

## Default settings

Zed sends the following settings to `typescript-ls` for both `typescript` and `javascript`. Override any of them under `lsp.typescript-ls.settings`; your values are merged over these defaults:

```json [settings]
{
  "lsp": {
    "typescript-ls": {
      "settings": {
        "typescript": {
          "suggest": { "completeFunctionCalls": true },
          "inlayHints": {
            "parameterNames": {
              "enabled": "all",
              "suppressWhenArgumentMatchesName": false
            },
            "parameterTypes": { "enabled": true },
            "variableTypes": {
              "enabled": true,
              "suppressWhenTypeMatchesName": false
            },
            "propertyDeclarationTypes": { "enabled": true },
            "functionLikeReturnTypes": { "enabled": true },
            "enumMemberValues": { "enabled": true }
          },
          "implementationsCodeLens": {
            "enabled": true,
            "showOnAllClassMethods": true,
            "showOnInterfaceMethods": true
          },
          "referencesCodeLens": {
            "enabled": true,
            "showOnAllFunctions": true
          }
        }
      }
    }
  }
}
```

The server only computes inlay hints and code lenses; Zed still decides whether to show them with the `inlay_hints.enabled` and `code_lens` settings:

```json [settings]
{
  "inlay_hints": { "enabled": true },
  "code_lens": "on"
}
```

Zed also formats TypeScript diagnostic messages, rendering quoted types as inline code or code blocks.

## Debugging

Zed supports debugging TypeScript code out of the box with `vscode-js-debug`.
The following can be debugged without writing additional configuration:

- Tasks from `package.json`
- Tests written using several popular frameworks (Jest, Mocha, Vitest, Jasmine, Node)

Run {#action debugger::Start} ({#kb debugger::Start}) to see a contextual list of these [predefined debug tasks](../debugger.md).

> **Note:** Bun test is automatically detected when `@types/bun` is present in `package.json`. Detected Bun tests can be run as tasks but not debugged, because `vscode-js-debug` does not support the WebKit Inspector Protocol that Bun uses.

> **Note:** Node test is automatically detected when `@types/node` is present in `package.json` (requires Node.js 20+).

As for all languages, configurations from `.vscode/launch.json` are also available for debugging in Zed.

If your use-case isn't covered by any of these, you can take full control by adding debug configurations to `.zed/debug.json`. See below for example configurations.

### Configuring JavaScript debug tasks

JavaScript debugging is more complicated than for other languages because there are two different environments: Node.js and the browser. `vscode-js-debug` exposes a `type` field, that you can use to specify the environment, either `node` or `chrome`.

- [vscode-js-debug configuration documentation](https://github.com/microsoft/vscode-js-debug/blob/main/OPTIONS.md)

### Attach debugger to a server running in web browser (`npx serve`)

Given an externally-run web server (e.g., with `npx serve` or `npx live-server`) one can attach to it and open it with a browser.

```json [debug]
[
  {
    "label": "Launch Chrome (TypeScript)",
    "adapter": "JavaScript",
    "type": "chrome",
    "request": "launch",
    "url": "http://localhost:5500",
    "program": "$ZED_FILE",
    "webRoot": "${ZED_WORKTREE_ROOT}",
    "build": {
      "command": "npx",
      "args": ["tsc"]
    },
    "skipFiles": ["<node_internals>/**"]
  }
]
```

## See also

- [Zed Yarn documentation](./yarn.md) for a walkthrough of configuring your project to use Yarn.
- [Zed Deno documentation](./deno.md)
