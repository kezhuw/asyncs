# Async runtime agnostic facilities

[![crates.io](https://img.shields.io/crates/v/asyncs)](https://crates.io/crates/asyncs)
[![github-ci](https://github.com/kezhuw/asyncs/actions/workflows/ci.yml/badge.svg?event=push)](https://github.com/kezhuw/asyncs/actions)
[![docs.rs](https://img.shields.io/docsrs/asyncs)](https://docs.rs/asyncs)
[![Apache-2.0](https://img.shields.io/github/license/kezhuw/asyncs)](https://github.com/kezhuw/asyncs/blob/master/LICENSE)

`asyncs` is a shim like package to ship async runtime agnostic facilities.

## Usage
```
[dependencies]
asyncs = "0.1.0"

[dev-dependencies]
asyncs = { version = "0.1.0", features = ["test"] }
```

Feature `test` should only be enabled for `dev-dependencies`.

## Provides
* `asyncs::task::spawn` to spawn tasks in runtime agnostic way from [spawns](https://docs.rs/spawns).
* `select!` to multiplex asynchronous futures simultaneously from [async-select](https://docs.rs/async-select).
* `#[asyncs::test]` to bootstrap a runtime for testing. This is only available with feature `test`.

## Does not provide
Executors.
