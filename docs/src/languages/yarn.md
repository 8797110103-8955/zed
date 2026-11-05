---
title: Yarn
description: "Configure Yarn language support in Zed, including language servers, formatting, and debugging."
---

# Yarn

[Yarn](https://yarnpkg.com/) is a JavaScript package manager that provides deterministic dependency resolution and offline caching.

## Setup

Zed's TypeScript language server (`typescript-ls`) only looks for the project's compiler in `node_modules/typescript`; Yarn Plug'n'Play SDKs in `.yarn/sdks` are not used.

1. Set `nodeLinker: node-modules` in `.yarnrc.yml` and run `yarn install`, so TypeScript is installed under `node_modules`.
2. Install TypeScript 7 or newer (`yarn add -D typescript@7`); older versions fall back to Zed's bundled TypeScript 7. See [TypeScript](./typescript.md#language-server).

After configuration, language server features (Go to Definition, completions, hover documentation) should work correctly.
