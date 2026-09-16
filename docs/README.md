---
title: Hemaka
description: Run AI agents like services — supervise their lifecycle, meter real token usage, and enforce dollar budgets, from a single Rust CLI.
---

# Hemaka Documentation

![Hemaka banner: run AI agents like services — supervise, meter, and budget them](assets/hemaka-banner.png)

Welcome to the Hemaka docs. Hemaka is a Rust CLI and engine that **runs AI agents like services**: register any agent, supervise its lifecycle, meter its real token usage, and enforce token and dollar budgets. These pages explain how to install it, run agents, configure them, and contribute.

By *agent* these pages mean a third-party program you run for yourself that calls a model on your behalf: a personal agent such as Hermes Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI. Hemaka runs that program as a supervised, metered process; it is not a framework for writing agents.

## Start Here

- [Getting started](get-started.md)
- [Installation](installation.md)
- [Command reference](commands.md)
- [Troubleshooting](troubleshooting.md)

## Concepts

- [Adapter Contract](adapter-contract.md)
- [Adapter manifest (`adapter.toml`)](manifest.md)
- [Supported agents](agents.md)
- [Embedding the engine](embedding.md)
- [Architecture](architecture.md)

## Design

- [Metering agents you don't control](design/metering-agents-you-dont-control.md)

## Project Workflows

- [Testing](testing.md)
- [Contributing](contributing.md)
- [Release process](release-process.md)
- [GitHub repository audit checklist](github-repository-audit-checklist.md)
- [Release notes](RELEASE_NOTES.md)
