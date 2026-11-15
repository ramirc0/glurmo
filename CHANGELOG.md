# Changelog

All notable changes to glurmo are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and glurmo uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `glurmo tui --me LOGIN`, and `me` in the config's `tui` section, name whose jobs `m` and the `Mine` panel show, as `?me=` does on the web. Before, the TUI always took `$USER`, which is wrong when following another site's server.

### Changed

- `record --pseudonymize` and `pseudonymize` also rewrite free-text reasons of jobs and nodes as `reason1`, `reason2` and so on, and every error's text as a fixed string. Reasons that are one word, as Slurm's own are, stay. Before, a job's reason could quote `user=` from an association message, a node's reason kept what an operator typed, and an error could quote raw squeue or scontrol output.
- Every TUI panel reads the filter as the jobs panel does. Each word must appear in a login, full name, partition, node, job ID, job name or reason. Before, the other panels looked for the whole text in one field, so `otter long` or a job ID showed nothing there.
- A node listing that names the same nodes sends only the nodes that changed. The full list goes out only when nodes join, leave or move. Before, any change sent every node. On zlurm's 15-minute fixture, node events shrink from 227 KB to 67 KB, and the whole stream from 994 KB to 834 KB.

### Removed

- The sampler no longer asks slurmctld for scheduler stats every minute, and the stream no longer carries them. No view showed them. `glurmo probe` still reads them. A config that sets `sample: (stats: ...)` must drop that key, or it fails to load.
- The protocol backend places jobs from the tick's job listing, so each tick sends slurmctld one job-info request instead of two.

### Fixed

- The command backend reads a job whose name holds newlines. squeue prints such a name over several lines. Before, one such job failed every job listing while it was listed.
- The command backend sets aside a job that scontrol prints unreadably, logs it, and saves it once to `unparsed.jsonl`. That covers a job on a node sinfo did not list, and a job whose name holds ` UserId=` or starts a line with `JobId=`. Before, one such job failed the whole placement listing, and a name holding ` UserId=` could set the job's start or CPU count.
- The command backend logs what a command that succeeds prints on stderr, once per text, and reads its output. Before, any stderr failed the call, so a host whose Slurm logs a warning on every command failed every call.
- The TUI wraps a node wider than its cores panel onto more lines, its GPUs and gauges lined up under its cores. Before, the panel cut it, and the `jobs` layout showed 7 of js029's 8 GPUs.
- A partition's free cores and GPUs leave out draining nodes, which take no new jobs. sinfo counts their idle CPUs as other. Before, they counted as free.
- Memory under a gigabyte reads in megabytes, as `512M`. Before, it read `0G`.
- A running job that is suspended or resumed, or whose time limit changes, updates in both views. A suspended job reads `S` or `susp` among your jobs, and `suspended` in the job list and its tip, with no countdown. Before, it kept the state and time limit it started with.
- The protocol backend skips the blank records a controller below 24.11 sends for FUTURE nodes and empty slots, as sinfo does. Before, one such record failed every node listing.
- The protocol backend reads deleted TRES too, as sacct does, so history counts the GPUs of jobs whose GPU TRES was deleted since. Before, those jobs read as holding none.
- The protocol backend signs slurmdbd credentials for any UID, as libslurm does. Before, it signed them for slurmctld's `SlurmUser`, so a slurmdbd running as another user refused every history read.

## [0.3.0] - 2026-11-15

### Changed

- The history chart redraws only when its usage, span, colors or size change, and at most once a frame while the wheel zooms or pans it. A replay moves a line over the chart instead of redrawing it. On zlurm, four seconds of replay at 60× redrew it 240 times before, and ten wheel steps redrew it ten times. Now they redraw it zero and two times.
- The jobs search filters 150 ms after the last key, so typing no longer waits on the list. On AICR each key rebuilt 1,300 rows before, which took 113 ms.
- History reads find live attempts through the index on ends or the one on submit times, whichever holds fewer rows to scan. On zlurm's store a rollup of the open bins took 150 ms and now takes under 6 ms.
- Each tick publishes its changes before it reads accounting, so a slow history read no longer delays live updates. The backfill grows its window after each read that works, up to a day, and waits a minute after one that fails. On zlurm it read 15 minutes at a time before.

### Fixed

- A short reply from SSSD fails the name lookup instead of panicking.
- A host or ID range over 65,536 values fails its listing instead of exhausting memory.
- The web view no longer crashes when a lit job row would be scrolled into view after its list was rebuilt.
- The editor turns accented letters in a name into plain ones for its file name, so `Rosé Pine Dawn` saves as `rose-pine-dawn`. Before, it dropped them.
- A replay that nears now no longer skips the changes past the server's clock. It asks only up to 15 s before now, where the live view takes over.
- The web view's banner counts up while the server or Slurm stays unreachable. Before, it showed how long it had been when the trouble began.
- Typing a layout's name in the editor keeps the field's focus. Before, each letter rebuilt the editor, and the next `e` closed it and dropped the draft.
- Hovering in the web view no longer slows down the longer a page stays open. Table rows read a copy of the highlight that goes away with them. Before, each rebuild of a table left its rows subscribed, and on AICR a hover took 7 ms on a fresh page and 385 ms after 120 rebuilds, as after a replay.
- `serve` stops cleanly on SIGINT or SIGTERM, even while it starts, and `tui` also on SIGHUP. `serve` leaves SIGHUP alone, so `nohup` still works. Before, `serve` left a temporary directory behind on every restart, and the TUI left the terminal in its alternate screen. `serve` makes that directory only for `--replay` now.
- The TUI quits on Ctrl-C while the filter is open. Before, Ctrl-C typed a `c`. It also draws a squeezed header or keys pane and a zero-hour history span, which panicked before.
- A job with no placement yet spreads leftover CPUs and GPUs over its first nodes. Before, each node drew the rounded-up share, so one GPU over two nodes drew two.
- The history chart's queue depth sums every user's waiting tasks before it rounds. Before, many short waits each rounded down to nothing.
- The protocol backend gives up on munged or sack after 15 s. Before, a daemon that accepted and never answered hung the sampler without an error.
- The command backend saves unparsed output to `unparsed.jsonl` once per kind of call in a run. Before, a listing that failed every tick appended its whole output each time, about 11 GB a day on zlurm.
- The command backend reads scontrol's `Job ... not found` as a job gone, and a lookup that fails moves to the back of the queue. Before, one array task the controller had dropped blocked every later lookup. With `lookups: 0`, starts no longer pile up in the queue.
- The command backend reads squeue's `NONE` end and `INVALID` time limit, scontrol's `No jobs in the system`, and a `NumCPUs` range such as `4-8`. Before, each failed its whole listing.
- GRES held without a device bitmap, such as `gpu:v100(CNT:2)`, and GRES counts in T or P no longer fail a whole job or node listing.
- A sample interval under a second is a config error. Before, `"0s"` or `"500ms"` made glurmo call slurmctld without pause.
- `/api/usage` and `/api/past/events` answer 400 to ranges whose length overflows, and `/api/usage` to times before 1970 or steps over a year. Before, a debug build panicked and a release build skipped the size limit.
- An empty or relative `XDG_*_HOME` falls back to its default under `$HOME`, as the XDG spec says. Before, an empty `XDG_STATE_HOME` put the store under the current directory.
- glurmo refuses to open a store whose schema is newer than it knows. Before, it opened it without a word.
- Pruning keeps each node's health as it stood at the cutoff. Before, a node drained for longer than `keep` showed as up in past views.
- Jobs stored before a run's first node listing get their cores counted once it arrives, and the bins they fell in are rolled up again. Before, history counted one core per CPU for them.
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

[Unreleased]: https://github.com/ramirc0/glurmo/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ramirc0/glurmo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ramirc0/glurmo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ramirc0/glurmo/releases/tag/v0.1.0
