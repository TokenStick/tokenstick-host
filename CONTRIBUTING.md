# Contributing

## Branches

| Branch | Purpose | Merges into | How |
| --- | --- | --- | --- |
| `main` | Released code only. Every commit on `main` is a tagged release. | - | Protected, PR only |
| `development` | Integration branch. Always builds and passes CI. Default target for PRs. | `main` (via release) | Protected, PR only |
| `feature/<name>` | New work, branched from `development` | `development` | PR, squash merge |
| `fix/<name>` | Bug fix for unreleased code, from `development` | `development` | PR, squash merge |
| `docs/<name>`, `chore/<name>`, `ci/<name>` | Docs, tooling, CI, from `development` | `development` | PR, squash merge |
| `release/vX.Y.Z` | Optional stabilization for a release, from `development` | `main`, then back into `development` | PR, merge commit |
| `hotfix/vX.Y.Z` | Urgent fix for a released version, from `main` | `main`, then back into `development` | PR, merge commit |

Names are short and kebab-case: `feature/pcie-vendor-backend`, `fix/sse-cancel`.

```
feature/*  ──squash──►  development  ──release PR──►  main  ──tag vX.Y.Z──►  GitHub release
                             ▲                          │
                             └──────── merge back ──────┘
```

## Versioning

[Semantic Versioning](https://semver.org). The single source of truth is `version` in the root `Cargo.toml` (`[workspace.package]`); every crate inherits it.

- Before 1.0: a **minor** bump (`0.2.0`) for each phase milestone or any breaking change to the `Backend` trait, CLI or API; a **patch** bump (`0.1.1`) for fixes.
- Release candidates: `vX.Y.Z-rc.N` tags on a `release/` branch, published as GitHub pre-releases.
- 1.0.0 ships with the first TokenStick hardware.

Planned milestones:

| Version | Content |
| --- | --- |
| 0.1.0 | Phase 0a: Backend trait, mock backend, CLI, OpenAI server, web UI, packaging |
| 0.2.0 | Phase 0d: `pcie-vendor` backend (RK1828 over PCIe, RKNN3) |
| 0.3.0 | Model store fed by tokenstick-models, Model Manager and Benchmark screens |
| 0.x | `usb` backend on TokenStick prototypes |
| 1.0.0 | First TokenStick hardware release |

## Releasing

1. On `development`: bump `version` in `Cargo.toml`, run `cargo build` to refresh `Cargo.lock`, move the `[Unreleased]` entries in `CHANGELOG.md` under the new version with today's date.
2. Open a PR `development` → `main` titled `Release vX.Y.Z`; merge with a merge commit once CI is green.
3. Tag `main`: `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.
4. The Release workflow checks the tag against `Cargo.toml`, builds x86_64 and aarch64 tarballs (binary, systemd unit, `install.sh`), and publishes a GitHub release with the changelog section.
5. Merge `main` back into `development`.

## Commits and pull requests

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `ci:`, `chore:`. Mark breaking changes with `!` (`feat!: ...`).
- PR titles use the same format; with squash merges the PR title becomes the commit on `development`.
- Every PR adds a line to `[Unreleased]` in `CHANGELOG.md` unless it is internal only.
- Before pushing: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`. CI runs the same checks plus an aarch64 build.

## Branch protection (repository settings)

`main` and `development`: require a pull request, require the CI checks (`fmt, clippy, test` and the aarch64 build) to pass, block force pushes and deletion. `main` additionally accepts merges only from `release/*`, `hotfix/*` or `development`.
