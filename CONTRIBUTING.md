# Contributing to Lumbridge Notes

Lumbridge is at v0.1 and kept by a small team. Please read this first.

## Before you start

- **Bugs:** open an issue. Say what you did, what happened, what you
  expected, your OS and terminal, and the output of
  `lumbridge-notes --version`.
- **Security problems:** do not open an issue. See [SECURITY.md](SECURITY.md).
- **Small fixes** (typos, clear bugs, docs): send a pull request.
- **Larger changes** (new features, new settings, a change to the note
  format): open an issue first and wait for a reply. We say no to many
  features to keep the editor small.

## Build and test

You need a stable Rust toolchain with `rustfmt` and `clippy`.

```sh
cargo build
scripts/ci.sh
```

`scripts/ci.sh` is what CI runs: the scrub check, `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings` and
`cargo test --locked`. Run it before you open a pull request.

## What we look for

- One change per pull request. Small is good.
- Tests for behaviour you change or add.
- The README and `--help` text updated in the same pull request when
  behaviour changes.
- A line in `CHANGELOG.md` under "Unreleased" for anything a user would
  notice.
- Plain words in docs, comments and messages. Short sentences.
- No new network calls, telemetry or update checks.
- No new dependency without a reason in the pull request text.

## The note format

`tests/golden/` pins the note format byte for byte, and Lumbridge TUI checks
the same files. Do not edit them by hand. If your change alters what
`src/store.rs` reads or writes, say so in the issue first.

## Never commit

- Passwords, tokens, keys or real `.env` files.
- Real names, e-mail addresses or notes of real people. Use made-up data:
  user `sam`, host `desk`, `example.com`.
- Private host names, private network addresses or internal links. For
  addresses use the documentation ranges, such as `192.0.2.1`.

`scripts/scrub-check` looks for these and fails the build. If it flags
something that is fine, add a line to `.scrub-allow` with the reason and say
so in the pull request.

## Third-party code

Say so in the pull request if any part of your change is copied or adapted
from somewhere else, and give the source and its licence. We cannot take code
under a licence that conflicts with Apache-2.0.

## Pull requests

- Write the commit subject as a short sentence that says what changed.
- In the pull request, say what the change does, why, and how you tested it.
- A maintainer reviews and merges.

## Licence of your work

By sending a contribution you agree that it is licensed under Apache-2.0, the
licence of this repository.

## Conduct

Be kind and direct. Argue about the work, not the person. Harassment and
personal attacks get you removed.
