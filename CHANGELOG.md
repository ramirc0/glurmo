# Changelog

All notable changes to glurmo are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and glurmo uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Each tick publishes its changes before it reads accounting, so a slow history read no longer delays live updates. The backfill grows its window after each read that works, up to a day, and waits a minute after one that fails. On zlurm it read 15 minutes at a time before.

### Fixed

- A job cancelled while pending counts as withdrawn. Before, the live listing counted it as started and ended, which made up 43% of zlurm's recorded starts.
- A failed call no longer discards the listings made before it in the same tick. Before, a failed job listing held back node and partition changes until their next interval, up to 10 minutes later.
- A failed accounting read no longer hides users' colors and names. Before, a store opened while slurmdbd was down drew every core in the muted color until a read worked.

## [0.2.0] - 2026-11-15

### Added

- Slurm 25.11's protocol beside 23.11's, so the protocol backend reaches controllers from 23.11 through 27.05. glurmo tries the newest first.
- `auth/slurm` sites: requests are signed through slurmd's or sackd's sack socket when munged does not run.

### Changed

- One accounting line or record that cannot be read is logged, saved once to `unparsed.jsonl`, and skipped. Before, it failed its whole read, and a backfill could stall on it.
- A drag on the history timeline highlights a stretch and zooms to it, instead of panning. The wheel still pans.

### Fixed

- The protocol backend folds pending tasks split out of an array back into the array's pending record, as squeue does.
- Accounting reads a job cancelled while pending as never started. Before, its start read as a date in 2106.
- The command backend counts the tasks of a stepped array range such as `3-15:2`. Before, one such job in `sacct` or `squeue` failed the whole read, and the history backfill retried that window forever.
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

[Unreleased]: https://github.com/ramirc0/glurmo/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ramirc0/glurmo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ramirc0/glurmo/releases/tag/v0.1.0
