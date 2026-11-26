# Changelog

All notable changes to glurmo are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and glurmo uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A way to serve the web view on a public hostname from a cluster that allows no inbound connections. `systemd/glurmo-tunnel.service` dials a chisel server over HTTPS and offers it the web view. `deploy/coolify` is that server as a Docker Compose stack, with an nginx in front that caches the page's files and still serves the page while the cluster is out of reach. Neither adds a login, so put one in front.

## [0.7.0] - 2026-11-25

### Added

- A gear at the top right of the web view opens a menu that picks the theme and the layout by name and opens the editor.
- A `Groups` panel lists user groups with what their members hold, in both views. In the web view, hovering a group lights its members' cores. Groups are the Unix groups of each job's owner, read from `/etc/group` and SSSD, and `groups` in the config keeps `only` some or leaves some out with `hide`.
- Palettes decide which colors the cores take, apart from the theme. A palette colors by user, by user group or by a job's time left, and binds colors to names. It hands the others to everyone in turn, as before, to the biggest holders only, or spread over every holder. The biggest are those who held the most over the stretch the history panel shows. Pick one with `p`, from the gear, with `?palette=` or with `tui: (palette: ...)`. `mono`, `okabe-ito`, `groups`, `viridis` and `time-left` are built in, and files in `palettes/` under the config directory add more.
- A palette tab in the web view's editor builds palettes: colors in order, filled from a colormap if wanted, names bound to a color, and the color of everyone else. It saves in the browser and exports RON, as the theme tab does.
- History keeps each job's time limit, so a replay under a time-left palette shows cores cooling off. An existing store reads its history from accounting once more, in the background, to fill in the limits of jobs stored before this.
- Under a time-left palette, the cores panel shows each step's hours. A job with more left than the span takes the last color, and one with no limit takes the rest color. The user and group tables show no color beside a name, and a row of the job list takes its own job's color.
- A replay can play at 3600×, an hour of history each second, from its heading or the key `5`.
- `reversed: true` in a palette, or *reverse the colors* in the editor, takes its colors last to first.

### Changed

- The history panel stacks users in the order glurmo first saw them, unless its `stack` option says `Biggest` or `Name`. Before, the biggest user over the loaded history sat at the bottom, so the bands changed places when the timeline was panned or resized. `Biggest` now counts the stretch shown.
- The history panel draws one bar per step of history at its own time, and steps start on fixed clock boundaries. Before, the chart sampled steps into fixed columns and the server grouped bins from wherever a request began, so its shape shifted as the timeline was panned or zoomed.
- The store's layout changed, for user groups and time limits. A store migrates when this version first opens it, and an older glurmo will not open it afterward.
- The `Die` panel is named `Tiles`. A layout that says `Die` still loads.
- The lines the protocol client logs, such as the protocol it settles on, carry the module `glurmo_rpc`. Before, they carried `glurmo::slurm::wire`.
- The units and `just timescale-check` run TimescaleDB 2.32.0 on PostgreSQL 18. To upgrade, pull `timescaledb-2.32.0-pg18.sif` beside the old image, install the new units, restart the database, and run `ALTER EXTENSION timescaledb UPDATE` in `psql -X` as the first command of a session.

## [0.6.0] - 2026-11-16

### Added

- The units append their logs to `~/.local/state/glurmo`: `serve.log`, `postgres.log` and `backup.log`. Each adds a dated line from systemd when its process ends, which records crashes and OOM kills.
- `glurmo-logrotate.timer` rotates those logs weekly, or once one passes 50 MB, and keeps a year of them compressed with zstd. `just install` installs its config as `~/.config/glurmo/logrotate.conf`.
- The Apptainer TimescaleDB logs lock waits over 1 s and statements over 5 s.
- TimescaleDB warnings reach glurmo's log as warnings. Its other notices, such as `relation "meta" already exists`, log at debug.
- `just install` builds the static binary and installs it as `~/.local/bin/glurmo` with the systemd user units. It then restarts glurmo if it is running.
- `systemd/glurmo-timescaledb.service` runs the Apptainer TimescaleDB the guide sets up. `glurmo-backup.timer` dumps it nightly to `~/.local/share/glurmo/glurmo.dump`.

### Changed

- A run of failures logs a warning when it starts and one line when 3 calls in a row have worked again, with how long it lasted and how many calls failed. That holds for each Slurm call, the store, and login lookups. Failures in between log at debug, where before each one logged a line, and a failing store two a tick.
- `/api/health` lists the store's writes as `store`, and each call's `slowest_ms` since the last hourly summary.
- Each hour the log sums up running and pending jobs, nodes that cannot run jobs, and for each kind of call how many were made and failed, the slowest, and the interval. A missing summary marks the server or host down.
- The log says when history has a gap and when accounting has filled it, when the TimescaleDB client gives up on a silent server and catches up, when it reconnects, and when the history task stops.
- Every message is a log line with the local time, its offset and a level. `GLURMO_LOG` sets the levels, such as `debug` or `glurmo=debug,warn`. Panics and fatal errors are logged too.
- `glurmo serve` logs its version, the cluster, the Slurm version and the store at start, the protocol and auth it settles on, `serving on` once the port is bound, and the signal that stops it. A request that fails with a 500 is logged. Accounting records that do not convert take one line per read, not one each.
- The guide walks through running glurmo as a service and setting up TimescaleDB step by step, including how to restore a dump.
- The guide's Apptainer TimescaleDB keeps its image and data in `$XDG_DATA_HOME/glurmo/timescaledb`.

### Fixed

- glurmo no longer starts before its TimescaleDB takes connections. `glurmo-timescaledb.service` counts as started only once `pg_isready` passes. Before, each start of both logged `cannot connect to TimescaleDB` and restarted glurmo 10 s later.
- A backup controller in standby, which answers with `ESLURM_IN_STANDBY_MODE`, now passes a call on to the next controller, as it does for libslurm. Before, after a failover and back, every call failed until a restart. A change of answering controller is logged.
- A log write that fails, as on a full disk or quota, no longer kills the server. Before, the first message after it panicked.
- The outage banner ends at the first listing that slurmctld answers. Before, it stayed up until the listing that failed last ran again, which backoff could put 80 minutes away for partitions.
- The guide's log drop-in spells out `~/.local/state`, because systemd 252 expands `%S` in a user unit to `~/.config`.
- A TimescaleDB server that stops answering, as a stopped process or a stuck disk does, no longer holds up the sampler. A call that waits 30 s checks the server on a new connection, and fails when that check gets no answer within 10 s. Before, the live view stopped until the server answered.
- A bin roll that fails after its tick is written is retried at the next tick. Before, the bins of attempts whose cores the first node listing counted kept their old totals until a restart.
- The guide's example config no longer names a store under `/scratch`. It documents the default, `$XDG_STATE_HOME/glurmo/<cluster>.sqlite`, and puts the Apptainer TimescaleDB under `$XDG_DATA_HOME`.

## [0.5.0] - 2026-11-15

### Added

- A logo, the foreman in `docs/media/logo.svg`. Its head and hard hat are the web view's favicon.
- History can live on a TimescaleDB server in place of the SQLite file. `store: (timescale: "host=db user=glurmo dbname=glurmo")` in the config picks it. Attempts and usage bins become hypertables in 30-day chunks, compressed once they are 7 days old where the server's license covers compression. SQLite stays the default, and its file does not change. A statement fails after 5 minutes, and a wait on a lock after 30 s, unless the connection string's `options` set `statement_timeout` or `lock_timeout`.
- `glurmo copy SQLITE URL` copies a SQLite store's history into an empty TimescaleDB database, and prints each table's row count on both sides. A server may keep writing to the SQLite store meanwhile. A copy that fails leaves the database empty, so it can run again. A server started on the copy carries on from it: users keep their colors, and accounting fills the gap since the copy.

### Fixed

- The web server keeps accepting connections while the store waits on a slow disk or a lock. Before, a store call that waited stopped it, because the sampler ran on the server's task.
- A tick that the store fails to write opens a gap, so accounting fills in the tick's starts and ends once writes work again. Accounting keeps no placements, so the tick's are lost. The tick is written whole or not at all, and no accounting is read while writes fail. Before, its starts and ends were lost for good. A node's health change that failed to be written was never written again, and a first node listing that failed left cores uncounted until a restart.
- `systemd/glurmo.service` starts only on the host its `ConditionHost` names, which its header asks you to set. Before, a home directory shared with compute nodes could start another server wherever a Slurm job started your systemd user manager.

## [0.4.0] - 2026-11-15

### Added

- `glurmo tui --me LOGIN`, and `me` in the config's `tui` section, name whose jobs `m` and the `Mine` panel show, as `?me=` does on the web. Before, the TUI always took `$USER`, which is wrong when following another site's server.

### Changed

- `record --pseudonymize` and `pseudonymize` also rewrite free-text reasons of jobs and nodes as `reason1`, `reason2` and so on, and every error's text as a fixed string. Reasons that are one word, as Slurm's own are, stay. Before, a job's reason could quote `user=` from an association message, a node's reason kept what an operator typed, and an error could quote raw squeue or scontrol output.
- Every TUI panel reads the filter as the jobs panel does. Each word must appear in a login, full name, partition, node, job ID, job name or reason. Before, the other panels looked for the whole text in one field, so `otter long` or a job ID showed nothing there.
- A node listing that names the same nodes sends only the nodes that changed. The full list goes out only when nodes join, leave or move. Before, any change sent every node. A TUI that follows a server must match its version. On zlurm's 15-minute fixture, node events shrink from 227 KB to 67 KB, and the whole stream from 994 KB to 834 KB.
- The protocol backend places jobs from the tick's job listing, so each tick sends slurmctld one job-info request instead of two.
- The protocol backend reads every field of each reply and fails one with bytes left over, so a layout it misreads shows as an error. It also checks the return code of slurmdbd's TRES list. Before, the configuration, scheduler statistics and TRES walks stopped early, and nothing checked what remained.
- The jobs list keeps the rows whose text has not changed, so a delta or a filter change builds only the rows that appear or change. On a replay with 1,170 rows, ten deltas rebuilt 12,446 rows before and 2,833 now. A search that every row matched rebuilt all 1,170 before and none now.
- History reads run on a task of their own, over a second connection to Slurm, so no tick waits for one. On zlurm, a fresh store's backfill kept jobs ticks 10 s apart while reads took up to 15 s. Before, each tick waited for the read, so reads of 8 to 13 s held ticks up to 20 s apart, and a hung slurmdbd held each tick for up to a minute.
- `/api/usage` sends each step as `[at, key, core_s, wait_s]` and leaves out steps with neither. On zlurm, two days by login shrank from 321 KB to 108 KB, and two weeks from 691 KB to 237 KB. Before, each step was an object that also carried GPU seconds, starts, ends and failures, which no view read. A TUI cannot read usage from a server of another version.

### Removed

- The sampler no longer asks slurmctld for scheduler stats every minute, and the stream no longer carries them. No view showed them. `glurmo probe` still reads them. A config that still sets `sample: (stats: ...)` loads, and the key is ignored.

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
- The protocol backend sends each call first to the controller that answered last. Before, every call waited up to 15 s on a dead primary before it tried a backup.
- Opening a store clears the start of ended attempts that never ran, which older versions stored, and rolls up their bins again. Before, they counted as starts, and those that seemed to start in 2106 stayed in the queue of a replay after they were cancelled. On zlurm's store this repairs 4,012 attempts, 1,009 of them in 2106, and takes 16 s once.
- Past views show a node that stopped answering the controller as no reply. History writes its health with a trailing `*`, as sinfo prints it. Before, history kept no flags, so such a node looked fine in the past. An older glurmo reads these records as unknown.
- The history charts' last column averages over the part of its step that has passed. Before, a step still under way was divided by its whole length, so the chart dipped at its right edge. The server also counts a running job in that step only up to the end asked for.
- The TUI keeps its usage graph when a read of it fails, notes the error below it, and tries again after 5 s. Before, the error replaced the graph until the next read a minute later.

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

[Unreleased]: https://github.com/ramirc0/glurmo/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/ramirc0/glurmo/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ramirc0/glurmo/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ramirc0/glurmo/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ramirc0/glurmo/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ramirc0/glurmo/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ramirc0/glurmo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ramirc0/glurmo/releases/tag/v0.1.0
