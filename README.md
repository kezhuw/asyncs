# Async runtime agnostic facilities

[![crates.io](https://img.shields.io/crates/v/asyncs)](https://crates.io/crates/asyncs)
[![github-ci](https://github.com/kezhuw/asyncs/actions/workflows/ci.yml/badge.svg?event=push)](https://github.com/kezhuw/asyncs/actions)
[![docs.rs](https://img.shields.io/docsrs/asyncs)](https://docs.rs/asyncs)
[![Apache-2.0](https://img.shields.io/github/license/kezhuw/asyncs)](https://github.com/kezhuw/asyncs/blob/master/LICENSE)

`asyncs` is a shim like package to ship async runtime agnostic facilities.

## Usages

* `cargo add asyncs` for libraries.
* `cargo add --dev --features test asyncs` for tests.
* `cargo add --features tokio,smol,async-global-executor` for binaries to compat with existing async runtimes. See [spawns][] for more.

Feature `test` should only be enabled for `dev-dependencies`.

## Provides
* `asyncs::task::spawn` to spawn tasks in runtime agnostic way from [spawns][].
* `select!` to multiplex asynchronous futures simultaneously from [async-select][].
* `#[asyncs::test]` to bootstrap a runtime for testing. This is only available with feature `test`.

## Does not provide
Executors and `#[asyncs::main]`.

[spawns]: https://docs.rs/spawns
[async-select]: https://docs.rs/async-select
