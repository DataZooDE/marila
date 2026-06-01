# VHS recordings

These tapes create short terminal recordings for the README/demo pages.
Install [VHS](https://github.com/charmbracelet/vhs), start the relevant
local services, then render the tape from the repository root:

```bash
vhs demo/vhs/vector.tape
vhs demo/vhs/tables.tape
vhs demo/vhs/embed-cli.tape
```

`vector.tape` and `tables.tape` are command tours. They intentionally
show the same boto3/AWS-protocol paths used by the demos instead of
mocking calls inside Rust.
