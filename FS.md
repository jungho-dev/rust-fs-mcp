# FS.md

## Purpose

* Use `rust-fs-mcp` first for local filesystem, search, git, and config work.
* Use shell only for build, test, runtime, package, network, or CLI behavior outside `rust-fs-mcp`.

## Rules

* Use absolute paths.
* Batch same-kind operations into one call.
* Use `allowMissing: true` when missing paths are expected.
* Prefer source files over generated, cached, vendor, build, log, backup, or temp artifacts.
* Use `args_path` or `*_path` fields for large arguments or file contents.
* Commit only when the user asks.

## Tool Routing

| Trigger                   | Use                                                         |
|---------------------------|-------------------------------------------------------------|
| Directory list/create     | `dir_list`, `dir_mk`                                        |
| File read/lines/metadata  | `file_read`, `file_lines`, `file_infos`                     |
| File write/edit           | `file_write`, `file_edit`                                   |
| File copy/move/remove     | `file_copy`, `file_move`, `file_remove`                     |
| Regex or broad search     | `search_regex`, `search_start`, `search_get`, `search_stop` |
| Git status/diff/show      | `git_status`, `git_diff`, `git_show`                        |
| Git add/commit            | `git_add`, `git_commit`                                     |
|---------------------------|-------------------------------------------------------------|
