# Third-party notices

## beachcomber

The design of star-forge — a daemon that computes statusline values once and serves cached
results to every caller over a Unix socket — comes from
[beachcomber](https://github.com/NavistAu/beachcomber).

Portions of `src/provider/git.rs` are adapted from
[beachcomber](https://github.com/NavistAu/beachcomber) (commit `e3c3bd2`,
`src/provider/git.rs`):

| star-forge | beachcomber |
|---|---|
| `git_root` | `find_repo_root` |
| `git_dir`, `common_git_dir` | `resolve_git_dir` |
| `git_state` | `detect_repo_state` |
| `git_stash_count` | `count_stashes` |
| `git_branch_from_head` | `GitHead::parse_head` |
| `parse_porcelain_v2` | `parse_git_status` |

```text
MIT License

Copyright (c) 2026 Joshua Hogendorn

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
