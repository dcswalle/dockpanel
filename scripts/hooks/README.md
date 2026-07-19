# Git hooks

These are the repository's checked-in hooks. They used to live only in
`.git/hooks/`, which meant they protected exactly one working copy — a fresh
clone, CI, or a second machine got nothing.

| Hook | Gates |
|---|---|
| `pre-commit` | Infrastructure-leak scan (real IPs, hostnames, tokens) |
| `pre-push` | Secrets scan, stale `panel/frontend/dist`, version consistency across the four packages, and `npm audit` + `cargo audit` across all six manifests |

## Install

```bash
git config core.hooksPath scripts/hooks
```

That points git at this directory instead of `.git/hooks`, so the hooks are
versioned with the code and every clone gets the same gates.

`cargo audit` is the part of the pre-push gate that catches what Dependabot
misses — it found four RustSec advisories in v2.11.1 (including a CVSS 9.1)
that Dependabot never surfaced. Install it once:

```bash
cargo install cargo-audit
```

Without it the pre-push gate degrades to a yellow warning rather than failing,
so check for that warning if you expect Rust advisories to be enforced.

## Design notes

- No gate ever early-exits; each sets `FOUND=1` and the hook drains at the end,
  so one push reports every problem instead of one per attempt.
- The audit gate **fails open** on network errors. `npm audit` and `cargo audit`
  both exit non-zero when they cannot reach their advisory source, which is
  indistinguishable from a real finding by exit code — a gate that blocks on a
  DNS blip is a gate that gets disabled with `--no-verify`.
- `cargo audit -n` uses the cached RustSec database. It is refreshed only by a
  run *without* `-n`, so do that occasionally.
