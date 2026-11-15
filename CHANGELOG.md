# Changelog

All notable changes to glurmo are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and glurmo uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- A drag on the history timeline highlights a stretch and zooms to it, instead of panning. The wheel still pans.

### Fixed

- Release builds embed the web view when `web/dist` appears or changes after an earlier build. Before, they could serve an empty page.
- The command backend reads jobs whose `SubmitLine` spans several lines, as an `sbatch --wrap` script with newlines does. Before, one such job failed every placement read.

## [0.1.0] - 2026-11-15

### Added

- A sampler that reads Slurm 23.11's protocol from slurmctld and slurmdbd, with munge authentication. It places every core of every job each tick.
- A fallback backend that runs `squeue`, `sinfo`, `scontrol`, `sdiag` and `sacct`.
- History in SQLite, with five-minute usage bins, a backfill from accounting, and recovery of jobs that ended while the sampler was down.
- `glurmo serve`, which streams the cluster's changes as server-sent events and resumes a dropped stream without a fresh snapshot.
- A web view with the cores and GPUs of every node, panels for users, the queue, partitions, node health, jobs and history, and a TV mode.
- A history timeline that replays the cluster from any past moment at 1× to 600×. It pans with the wheel, zooms with shift or ctrl and the wheel, and marks the moment under the pointer.
- A layout and theme editor in the web view, with base16 import and RON export.
- `glurmo tui`, a terminal view that follows a server or samples the cluster itself.
- Three Rosé Pine themes, 20 base16 themes, and four layouts.
- `glurmo probe`, which reports what the cluster allows and what each call costs the controller.
- `glurmo record`, `pseudonymize`, `convert` and `serve --replay --speed`, for recordings, demos and test fixtures.
- Account lookups through `/etc/passwd` and SSSD's socket, so the static binary sees directory users.

[Unreleased]: https://github.com/ramirc0/glurmo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ramirc0/glurmo/releases/tag/v0.1.0
