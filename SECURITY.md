# Security policy

## Reporting a problem

Please do not open a public issue, pull request or discussion for a security
problem.

Email **security@lumbridgecorp.com**.

Please include:

- The version or commit (`lumbridge-notes --version`).
- Your operating system, CPU type and terminal.
- What you did, what happened, and what you expected.
- The impact you think it has.
- A small way to reproduce it, if you have one.

Remove passwords, tokens and private notes from anything you send.

## What happens next

- We reply to say we have your report.
- We tell you whether we can reproduce it and what we plan to do.
- We keep you informed until it is fixed or closed.
- We credit you in the change log when the fix ships, unless you ask us not
  to.

Please give us a fair chance to fix the problem before you publish it. We ask
for 90 days, or less if a fix is out sooner.

## Supported versions

Only the latest commit on `main` is supported. There are no back-ports.

## How Lumbridge Notes is meant to be used

- It runs as you and reads and writes files you can already reach. It opens no
  network port.
- It contacts a server only when you set one in `config.toml` and press the
  key for it. The note text, the selection or your recording then goes to that
  server.
- A token for the AI server is read from the environment variable you name in
  `api_key_env`. It is not stored by the editor.
- Live dictation uses plain `ws://`. Use it on a network you trust.
- The assistants run `curl` and the audio commands found on `PATH`, or the
  commands you set in `config.toml`.

## What counts

In scope:

- A note, a template, a prompt file or a server reply that makes the editor
  run a command, or read or write a file outside the notebook.
- A leak of a token or of note text to a log, another user or a server you
  did not set.
- Loss or corruption of a note on save.

Out of scope:

- Anything that needs an attacker who already has your user account.
- What a server you configured does with the text you send it.
- Commands you put in `config.toml` yourself.

## Good faith

We will not take legal action against anyone who finds and reports a problem
in good faith, keeps to this policy, and does not access or change other
people's data.
