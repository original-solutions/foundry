# The runner's run file

`original-solutions__foundry__179__1.jsonl` is the run file the runner wrote for issue #179 of `original-solutions/foundry`, attempt 1. factory-runner v0.3.7 wrote it, at commit `eeee47f237f02bceb201a04a84fe4c52edd06076` of `original-solutions/factory-on-paradev`.

It is copied byte for byte from the runner's own ledger, `~/.config/factory-runner/runs/`, and was read for anything private before it was committed. Do not edit or reformat it: the export test (`factory_export_writes_the_runners_own_run_file_byte_for_byte` in `crates/foundryd/tests/cli.rs`) loads its events into a store, runs `foundryd factory export`, and compares the file it writes with this one byte for byte.

To replace it, copy another finished run file from `~/.config/factory-runner/runs/` the same way, read it for anything private, and change the run, the runner version and the commit named here.
