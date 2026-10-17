# Stability and compatibility policy

gauge follows [Semantic Versioning](https://semver.org/). This page says what
that means in practice: what you can rely on across releases, and what you
can't. **It has been in effect since 1.0.0.** (Releases before that, 0.x, made
no such promise.)

## What is stable

Within a major version (1.x), these change only in backward-compatible ways.
Breaking one of them needs a new major version.

| Surface | What is guaranteed |
|---|---|
| **Command line** | The names and meaning of subcommands (`client`, `server`) and of the documented flags and their short forms. New flags may appear; existing ones are not removed or repurposed. Exit code `0` means success, non-zero a failure to run the test. |
| **Config file** (`config.toml`) | Existing keys and their meaning. New keys may appear; unknown keys in a file are ignored. Precedence stays *flag > config > built-in default*. |
| **`history.json`** and the **`-J` output** | Existing fields keep their name, type and meaning. New fields may be added (older files without them still load); fields are not removed. |
| **Wire protocol** | See [protocol.md](protocol.md#versioning-and-compatibility): a 1.x server serves every earlier protocol version, a 1.x client works with older servers except where a feature needs a newer protocol. The protocol version only ever increases. |
| **Platforms** | Linux and macOS. Dropping a platform is a major change. (Windows is not supported.) |

## What may change in any release

- The **look of the terminal UI**: layout, wording, colours, which values are
  shown where. Tests may be added or reworded.
- The **human-readable CLI output** (the lines `gauge client`/`server` print).
  Scripts should use `-J`, not scrape text.
- The format of **`gauge.log`**, which is for people.
- **How a number is measured**, when it is a fix or an improvement (for
  example pacing precision, or what counts as a probe passing in the rate
  search). Such changes are listed in the changelog. A result from one version
  is not guaranteed to equal the result of another.
- The **minimum supported Rust version** (currently 1.88). Raising it is a
  minor release and is called out in the changelog.
- Anything in the **Rust code**: gauge is a binary, not a library; there is no
  stable Rust API.

## Deprecation

A flag, config key or output field that is going to be removed is first
**deprecated** in a minor release — it keeps working, prints a notice where
that is possible, and the changelog names its replacement — and removed no
earlier than the next major release.

## Reporting a compatibility problem

If an upgrade within a major version broke something listed under "What is
stable", that is a bug: please open an issue with the two versions and the
command line. Security issues go through [SECURITY.md](../SECURITY.md).
